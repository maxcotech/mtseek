//! Writing a segment, to a file or into memory.

use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};

use crate::format::{
    self, BLOCK_TARGET, END_MAGIC, HEADER_LEN, MAGIC, NO_BLOCK, SECTIONS, UTF16_PATH, VERSION,
    ZSTD_LEVEL,
};
use crate::{EXTERNAL_BLOCK, Entry, IndexError, MAX_STORED, Meta, parallel_map, text, trigram};

/// One file to put in the index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Doc {
    /// Which root of the scope it was found under.
    pub root: u32,
    /// Its path relative to that root.
    pub path: PathBuf,
    /// Its NTFS file reference number, or 0 if unknown (ADR 0011).
    pub frn: u64,
    /// The filters whose walk lists it ([`crate::membership`]). The writer sets
    /// [`crate::membership::TRUNCATED`] itself, from the content.
    pub flags: u8,
    /// What the index holds for it.
    pub content: Content,
}

/// Everything a segment holds besides its files' bytes.
#[derive(Debug, Clone, Copy)]
pub struct SegmentInfo<'a> {
    /// The scope description ([`crate::scope_key`]).
    pub key: &'a str,
    /// The number of roots in the scope.
    pub roots: u32,
    /// Files and directories that couldn't be read.
    pub errors: &'a [StoredError],
    /// Directories the walk entered.
    pub dirs: &'a [Entry],
    /// Ignore-control files in those directories.
    pub controls: &'a [Entry],
    /// In a delta: the base files it replaces or deletes, in increasing order.
    pub tombstones: &'a [u32],
    /// Build id, base id and journal checkpoints.
    pub meta: &'a Meta,
}

/// What the index holds for a file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Content {
    /// The bytes to store: the file, cut as [`text::stored_len`] allows. At most
    /// [`MAX_STORED`] bytes.
    Stored(Vec<u8>),
    /// Too big to store. It is read from disk by every query that runs.
    External,
}

/// A file or directory that couldn't be read while building. Every query reports it again,
/// as `rg` would.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredError {
    /// The root the path is relative to, or `None` if the path is complete as it is.
    pub root: Option<u32>,
    /// The path.
    pub path: PathBuf,
    /// The error message.
    pub message: String,
    /// The filters whose walk reports it ([`crate::membership`]).
    pub flags: u8,
}

/// Sizes of a written segment.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WriteStats {
    /// Files in the index.
    pub files: u64,
    /// Files too big to store, read from disk at query time.
    pub external: u64,
    /// Bytes stored before compression.
    pub stored_bytes: u64,
    /// Bytes of the compressed store.
    pub compressed_bytes: u64,
    /// Distinct trigrams.
    pub trigrams: u64,
    /// Trigram postings (one per distinct trigram per file).
    pub postings: u64,
    /// Size of the segment file.
    pub file_bytes: u64,
}

/// Writes `docs` as a segment at `path`, replacing any file there only once the new one is
/// complete and flushed to disk. `docs` is sorted by root and path first, which is the
/// order queries report files in.
pub fn write_segment(
    path: &Path,
    info: &SegmentInfo<'_>,
    docs: &mut [Doc],
    threads: usize,
) -> Result<WriteStats, IndexError> {
    encode(path, info, docs, threads, |header, sections| {
        write_atomically(path, header, sections)
    })
}

/// The bytes [`write_segment`] would write to `path`, kept in memory (ADR 0013: the
/// daemon's delta before it is written). `path` only names the segment in errors.
pub fn segment_bytes(
    path: &Path,
    info: &SegmentInfo<'_>,
    docs: &mut [Doc],
    threads: usize,
) -> Result<(Vec<u8>, WriteStats), IndexError> {
    let mut bytes = Vec::new();
    let stats = encode(path, info, docs, threads, |header, sections| {
        emit(&mut bytes, header, sections)
    })?;
    Ok((bytes, stats))
}

type Sections<'a> = [&'a [&'a [u8]]; SECTIONS];

/// Builds the sections of a segment and hands them to `sink`, which returns the size.
fn encode(
    path: &Path,
    info: &SegmentInfo<'_>,
    docs: &mut [Doc],
    threads: usize,
    sink: impl FnOnce(&Header, &Sections<'_>) -> io::Result<u64>,
) -> Result<WriteStats, IndexError> {
    let fits = |n: usize| u32::try_from(n).map_err(|_| too_big(path));
    let roots = info.roots;
    docs.sort_by(|a, b| (a.root, &a.path).cmp(&(b.root, &b.path)));
    let entries_ok = info
        .dirs
        .iter()
        .chain(info.controls)
        .all(|e| e.root < roots);
    let tombstones_ok = info.tombstones.windows(2).all(|w| w[0] < w[1]);
    if !entries_ok || !tombstones_ok {
        return Err(IndexError::Corrupt {
            path: path.to_path_buf(),
            reason: "bad directory, control or tombstone entries",
        });
    }
    if docs.iter().any(|d| d.root >= roots)
        || docs
            .iter()
            .any(|d| matches!(&d.content, Content::Stored(b) if b.len() as u64 > MAX_STORED))
    {
        return Err(IndexError::Corrupt {
            path: path.to_path_buf(),
            reason: "a file has a bad root or is too big to store",
        });
    }

    // Trigrams of each file, in parallel, then the postings.
    let per_doc: Vec<Vec<u32>> = parallel_map(docs, threads, |doc| {
        let mut keys = Vec::new();
        if let Content::Stored(bytes) = &doc.content {
            trigram::distinct(&text::matcher_text(bytes), &mut keys);
        }
        keys
    });
    let postings = Postings::build(&per_doc);
    drop(per_doc);

    let packed = pack(docs, path)?;
    let compressed = compress(&packed.blocks, threads).map_err(|source| IndexError::Io {
        path: path.to_path_buf(),
        source,
    })?;

    let header = Header {
        roots,
        docs: fits(docs.len())?,
        blocks: fits(packed.blocks.len())?,
        trigrams: fits(postings.trigrams)?,
        errors: fits(info.errors.len())?,
    };
    let error_bytes = error_records(info.errors);
    let frames: Vec<&[u8]> = compressed.frames.iter().map(Vec::as_slice).collect();
    let mut dirs = Vec::new();
    for entry in info.dirs {
        format::put_entry(&mut dirs, entry);
    }
    let mut controls = Vec::new();
    for entry in info.controls {
        format::put_entry(&mut controls, entry);
    }
    let tombstones: Vec<u8> = info
        .tombstones
        .iter()
        .flat_map(|t| t.to_le_bytes())
        .collect();
    let meta = format::put_meta(info.meta);
    let sections: Sections<'_> = [
        &[info.key.as_bytes()],
        &[&packed.docs],
        &[&packed.paths],
        &[&error_bytes],
        &[&postings.dictionary],
        &[&postings.lists],
        &[&compressed.table],
        &frames,
        &[&dirs],
        &[&controls],
        &[&tombstones],
        &[&meta],
    ];
    let file_bytes = sink(&header, &sections).map_err(|source| IndexError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    Ok(WriteStats {
        files: docs.len() as u64,
        external: packed.external,
        stored_bytes: packed.stored_bytes,
        compressed_bytes: compressed.bytes,
        trigrams: postings.trigrams as u64,
        postings: postings.count as u64,
        file_bytes,
    })
}

struct Header {
    roots: u32,
    docs: u32,
    blocks: u32,
    trigrams: u32,
    errors: u32,
}

/// The dictionary and postings sections.
struct Postings {
    dictionary: Vec<u8>,
    lists: Vec<u8>,
    trigrams: usize,
    count: usize,
}

impl Postings {
    /// Builds both sections from each file's sorted trigram keys (file id = position).
    fn build(per_doc: &[Vec<u32>]) -> Self {
        // Count files per trigram in a dense table (keys are 24-bit; the zeroed allocation
        // is lazily mapped, so only used entries cost anything), noting each trigram the
        // first time it appears. Then turn the counts into a slot map over the distinct
        // trigrams, in key order. Counts never exceed the file count and slots never exceed
        // 2^24: both fit in u32.
        let mut slot = vec![0u32; 1 << 24];
        let mut keys = Vec::new();
        for doc_keys in per_doc {
            for &k in doc_keys {
                let count = &mut slot[k as usize];
                if *count == 0 {
                    keys.push(k);
                }
                *count += 1;
            }
        }
        keys.sort_unstable();
        let mut starts = Vec::with_capacity(keys.len());
        let mut total = 0usize;
        for (s, &k) in (0u32..).zip(&keys) {
            starts.push(total);
            total += slot[k as usize] as usize;
            slot[k as usize] = s;
        }
        // Files are visited in order, so every trigram's list comes out sorted.
        let mut ids = vec![0u32; total];
        let mut fill = starts.clone();
        for (doc, doc_keys) in (0u32..).zip(per_doc) {
            for &k in doc_keys {
                let s = slot[k as usize] as usize;
                ids[fill[s]] = doc;
                fill[s] += 1;
            }
        }
        drop(slot);

        let mut dictionary = Vec::with_capacity(keys.len() * format::DICT_LEN);
        let mut lists = Vec::with_capacity(total * 2);
        for ((&k, &start), &end) in keys.iter().zip(&starts).zip(&fill) {
            let list = &ids[start..end];
            dictionary.extend_from_slice(&k.to_le_bytes());
            // A list is never longer than the file count, which fits in u32.
            dictionary
                .extend_from_slice(&u32::try_from(list.len()).unwrap_or(u32::MAX).to_le_bytes());
            dictionary.extend_from_slice(&(lists.len() as u64).to_le_bytes());
            let mut previous = 0;
            for &id in list {
                format::put_varint(&mut lists, id - previous);
                previous = id;
            }
        }
        Self {
            dictionary,
            lists,
            trigrams: keys.len(),
            count: total,
        }
    }
}

/// Files packed into blocks, with the docs and paths sections that locate them.
struct Packed {
    blocks: Vec<Vec<u8>>,
    docs: Vec<u8>,
    paths: Vec<u8>,
    external: u64,
    stored_bytes: u64,
}

/// Packs stored files into blocks of about [`BLOCK_TARGET`] bytes, in order. A file at least
/// that big gets a block of its own. Takes the bytes out of `docs`.
fn pack(docs: &mut [Doc], path: &Path) -> Result<Packed, IndexError> {
    let fits = |n: usize| u32::try_from(n).map_err(|_| too_big(path));
    let mut packed = Packed {
        blocks: Vec::new(),
        docs: Vec::with_capacity(docs.len() * format::DOC_LEN),
        paths: Vec::new(),
        external: 0,
        stored_bytes: 0,
    };
    let mut current = Vec::new();
    for doc in docs.iter_mut() {
        let truncated = match &doc.content {
            Content::Stored(bytes) => text::is_cut(bytes),
            Content::External => false,
        };
        let flags = (doc.flags & !crate::membership::TRUNCATED)
            | if truncated {
                crate::membership::TRUNCATED
            } else {
                0
            };
        let (block, offset, len) = match &mut doc.content {
            Content::External => {
                packed.external += 1;
                (EXTERNAL_BLOCK, 0, 0)
            }
            Content::Stored(bytes) if bytes.is_empty() => (NO_BLOCK, 0, 0),
            Content::Stored(bytes) => {
                packed.stored_bytes += bytes.len() as u64;
                let len = fits(bytes.len())?;
                let bytes = std::mem::take(bytes);
                if bytes.len() >= BLOCK_TARGET {
                    if !current.is_empty() {
                        packed.blocks.push(std::mem::take(&mut current));
                    }
                    packed.blocks.push(bytes);
                    (block_number(packed.blocks.len() - 1, path)?, 0, len)
                } else {
                    let at = (
                        block_number(packed.blocks.len(), path)?,
                        fits(current.len())?,
                        len,
                    );
                    current.extend_from_slice(&bytes);
                    if current.len() >= BLOCK_TARGET {
                        packed.blocks.push(std::mem::take(&mut current));
                    }
                    at
                }
            }
        };
        let (path_bytes, utf16) = format::encode_path(&doc.path);
        let path_offset = fits(packed.paths.len())?;
        let path_len = fits(path_bytes.len())?;
        if path_len >= UTF16_PATH {
            return Err(too_big(path));
        }
        packed.paths.extend_from_slice(&path_bytes);
        let flag = if utf16 { UTF16_PATH } else { 0 };
        for field in [block, offset, len, path_offset, path_len | flag, doc.root] {
            packed.docs.extend_from_slice(&field.to_le_bytes());
        }
        packed.docs.extend_from_slice(&doc.frn.to_le_bytes());
        packed
            .docs
            .extend_from_slice(&u32::from(flags).to_le_bytes());
    }
    if !current.is_empty() {
        packed.blocks.push(current);
    }
    Ok(packed)
}

/// The compressed store and the block table that locates each frame in it.
struct Compressed {
    frames: Vec<Vec<u8>>,
    table: Vec<u8>,
    bytes: u64,
}

fn compress(blocks: &[Vec<u8>], threads: usize) -> io::Result<Compressed> {
    let frames: Vec<io::Result<Vec<u8>>> = parallel_map(blocks, threads, |block| {
        zstd::bulk::compress(block, ZSTD_LEVEL)
    });
    let mut out = Compressed {
        frames: Vec::with_capacity(blocks.len()),
        table: Vec::with_capacity(blocks.len() * format::BLOCK_LEN),
        bytes: 0,
    };
    for (raw, frame) in blocks.iter().zip(frames) {
        let frame = frame?;
        // Blocks hold at most MAX_STORED (256 MiB) bytes, and zstd frames of incompressible
        // data are barely bigger, so both lengths fit in u32.
        let lengths = [frame.len(), raw.len()].map(|n| u32::try_from(n).unwrap_or(u32::MAX));
        out.table.extend_from_slice(&out.bytes.to_le_bytes());
        out.table.extend_from_slice(&lengths[0].to_le_bytes());
        out.table.extend_from_slice(&lengths[1].to_le_bytes());
        out.bytes += frame.len() as u64;
        out.frames.push(frame);
    }
    Ok(out)
}

fn error_records(errors: &[StoredError]) -> Vec<u8> {
    let mut out = Vec::new();
    for error in errors {
        let (path, utf16) = format::encode_path(&error.path);
        // Paths and messages are short; anything absurdly long is cut to fit.
        let path_len = u32::try_from(path.len()).unwrap_or(0).min(UTF16_PATH - 1);
        let message = error.message.as_bytes();
        let message_len = u32::try_from(message.len()).unwrap_or(0);
        out.extend_from_slice(&error.root.unwrap_or(u32::MAX).to_le_bytes());
        out.extend_from_slice(&u32::from(error.flags).to_le_bytes());
        out.extend_from_slice(&(path_len | if utf16 { UTF16_PATH } else { 0 }).to_le_bytes());
        out.extend_from_slice(&path[..path_len as usize]);
        out.extend_from_slice(&message_len.to_le_bytes());
        out.extend_from_slice(&message[..message_len as usize]);
    }
    out
}

/// Writes header, sections and trailer to a temporary file, flushes it to disk and renames
/// it over `path`. Returns the file size.
fn write_atomically(path: &Path, header: &Header, sections: &Sections<'_>) -> io::Result<u64> {
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    let result = (|| {
        let file = File::create(&tmp)?;
        let mut out = BufWriter::with_capacity(1 << 20, file);
        let size = emit(&mut out, header, sections)?;
        let file = out.into_inner().map_err(io::IntoInnerError::into_error)?;
        file.sync_all()?;
        Ok(size)
    })();
    match result {
        Ok(size) => {
            std::fs::rename(&tmp, path)?;
            Ok(size)
        }
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}

/// Writes `bytes` (a segment made by [`segment_bytes`]) to `path` the way
/// [`write_segment`] does: complete and flushed to disk before it replaces anything.
pub fn write_bytes(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    let result = File::create(&tmp).and_then(|mut file| {
        file.write_all(bytes)?;
        file.sync_all()
    });
    match result {
        Ok(()) => std::fs::rename(&tmp, path),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}

/// Writes header, sections and trailer to `out`. Returns the size.
fn emit(out: &mut impl Write, header: &Header, sections: &Sections<'_>) -> io::Result<u64> {
    out.write_all(MAGIC)?;
    for field in [
        VERSION,
        header.roots,
        header.docs,
        header.blocks,
        header.trigrams,
        header.errors,
    ] {
        out.write_all(&field.to_le_bytes())?;
    }
    let mut at = HEADER_LEN as u64;
    let mut table = Vec::with_capacity(SECTIONS * 16);
    for parts in sections {
        let len: u64 = parts.iter().map(|p| p.len() as u64).sum();
        table.extend_from_slice(&at.to_le_bytes());
        table.extend_from_slice(&len.to_le_bytes());
        for part in *parts {
            out.write_all(part)?;
        }
        at += len;
    }
    out.write_all(&table)?;
    out.write_all(END_MAGIC)?;
    Ok(at + table.len() as u64 + END_MAGIC.len() as u64)
}

fn block_number(n: usize, path: &Path) -> Result<u32, IndexError> {
    u32::try_from(n)
        .ok()
        .filter(|&b| b < EXTERNAL_BLOCK)
        .ok_or_else(|| too_big(path))
}

fn too_big(path: &Path) -> IndexError {
    IndexError::Corrupt {
        path: path.to_path_buf(),
        reason: "the scope is too big for one segment",
    }
}
