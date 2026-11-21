//! Opening and reading a segment file.
//!
//! Small sections are read whole when the segment opens and checked there, so the accessors
//! below can't go out of bounds. Postings and store blocks are read with positioned reads
//! only when a query needs them, and checked as they are decoded.

use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};

use crate::format::{
    self, BLOCK_LEN, DICT_LEN, DOC_LEN, END_MAGIC, HEADER_LEN, MAGIC, NO_BLOCK, SECTIONS,
    TRAILER_LEN, UTF16_PATH, VERSION, section, u32_at, u64_at,
};
use crate::{EXTERNAL_BLOCK, Entry, IndexError, MAX_STORED, Meta, StoredError};

/// An open segment.
#[derive(Debug)]
pub struct Segment {
    path: PathBuf,
    file: File,
    key: String,
    roots: u32,
    doc_count: u32,
    docs: Vec<u8>,
    paths: Vec<u8>,
    dictionary: Vec<u8>,
    blocks: Vec<u8>,
    errors: Vec<StoredError>,
    tombstones: Vec<u32>,
    meta: Meta,
    postings: (u64, u64),
    store: (u64, u64),
    dirs: (u64, u64),
    controls: (u64, u64),
    file_bytes: u64,
}

/// Where a file's bytes are.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Location {
    /// The file is empty.
    Empty,
    /// Too big to store: read it from disk.
    External,
    /// `len` bytes at `offset` in store block `block`.
    Block {
        /// The block number.
        block: u32,
        /// Offset of the file in the block's uncompressed bytes.
        offset: u32,
        /// Stored length.
        len: u32,
    },
}

impl Segment {
    /// Opens and checks the segment at `path`.
    pub fn open(path: &Path) -> Result<Self, IndexError> {
        let corrupt = |reason| IndexError::Corrupt {
            path: path.to_path_buf(),
            reason,
        };
        let io_err = |source| IndexError::Io {
            path: path.to_path_buf(),
            source,
        };
        let file = File::open(path).map_err(io_err)?;
        let file_bytes = file.metadata().map_err(io_err)?.len();
        if file_bytes < (HEADER_LEN + TRAILER_LEN) as u64 {
            return Err(corrupt("too short"));
        }
        let mut header = [0u8; HEADER_LEN];
        read_at(&file, &mut header, 0).map_err(io_err)?;
        if &header[..8] != MAGIC {
            return Err(corrupt("not a content index"));
        }
        if u32_at(&header, 8) != VERSION {
            return Err(corrupt("another format version"));
        }
        let [roots, doc_count, block_count, trigram_count, error_count] =
            [12, 16, 20, 24, 28].map(|at| u32_at(&header, at));
        let ranges = section_ranges(&file, file_bytes).map_err(corrupt)?;

        let read_section = |i: usize| -> Result<Vec<u8>, IndexError> {
            let (offset, len) = ranges[i];
            let len = usize::try_from(len).map_err(|_| corrupt("section too big"))?;
            let mut buf = vec![0u8; len];
            read_at(&file, &mut buf, offset).map_err(io_err)?;
            Ok(buf)
        };
        let key = String::from_utf8(read_section(section::SCOPE)?)
            .map_err(|_| corrupt("scope is not text"))?;
        let docs = read_section(section::DOCS)?;
        let paths = read_section(section::PATHS)?;
        let error_bytes = read_section(section::ERRORS)?;
        let dictionary = read_section(section::DICTIONARY)?;
        let blocks = read_section(section::BLOCKS)?;
        let tombstone_bytes = read_section(section::TOMBSTONES)?;
        let meta =
            format::parse_meta(&read_section(section::META)?).ok_or_else(|| corrupt("bad meta"))?;

        if docs.len() as u64 != u64::from(doc_count) * DOC_LEN as u64
            || dictionary.len() as u64 != u64::from(trigram_count) * DICT_LEN as u64
            || blocks.len() as u64 != u64::from(block_count) * BLOCK_LEN as u64
        {
            return Err(corrupt("section sizes disagree with the header"));
        }
        let store = ranges[section::STORE];
        let postings = ranges[section::POSTINGS];
        let block_raw = check_blocks(&blocks, store.1).map_err(corrupt)?;
        check_docs(&docs, &paths, &block_raw, roots).map_err(corrupt)?;
        check_dictionary(&dictionary, doc_count, postings.1).map_err(corrupt)?;
        let errors =
            parse_errors(&error_bytes, error_count, roots).ok_or_else(|| corrupt("bad errors"))?;
        let (pairs, rest) = tombstone_bytes.as_chunks::<4>();
        let tombstones: Vec<u32> = pairs.iter().map(|&b| u32::from_le_bytes(b)).collect();
        if !rest.is_empty() || tombstones.windows(2).any(|w| w[0] >= w[1]) {
            return Err(corrupt("bad tombstones"));
        }

        Ok(Self {
            path: path.to_path_buf(),
            file,
            key,
            roots,
            doc_count,
            docs,
            paths,
            dictionary,
            blocks,
            errors,
            tombstones,
            meta,
            postings,
            store,
            dirs: ranges[section::DIRS],
            controls: ranges[section::CONTROLS],
            file_bytes,
        })
    }

    /// The scope description the segment was built for (see [`crate::scope_key`]).
    pub fn key(&self) -> &str {
        &self.key
    }

    /// The number of roots in the scope.
    pub fn roots(&self) -> usize {
        self.roots as usize
    }

    /// The number of files.
    pub fn len(&self) -> u32 {
        self.doc_count
    }

    /// Whether the segment holds no files.
    pub fn is_empty(&self) -> bool {
        self.docs.is_empty()
    }

    /// The size of the segment file.
    pub fn file_bytes(&self) -> u64 {
        self.file_bytes
    }

    /// Files and directories that couldn't be read when the segment was built.
    pub fn errors(&self) -> &[StoredError] {
        &self.errors
    }

    /// For a delta: the base's files it replaces or deletes, in increasing order.
    pub fn tombstones(&self) -> &[u32] {
        &self.tombstones
    }

    /// Build id, base id and journal checkpoints.
    pub fn meta(&self) -> &Meta {
        &self.meta
    }

    /// The directories the walk entered (read from disk on each call).
    pub fn dirs(&self) -> Result<Vec<Entry>, IndexError> {
        self.entries(self.dirs)
    }

    /// The ignore-control files in those directories (read from disk on each call).
    pub fn controls(&self) -> Result<Vec<Entry>, IndexError> {
        self.entries(self.controls)
    }

    /// `doc`'s NTFS file reference number, or 0 if unknown.
    ///
    /// # Panics
    ///
    /// If `doc` is not less than [`Segment::len`].
    pub fn frn(&self, doc: u32) -> u64 {
        u64_at(self.doc(doc), 24)
    }

    /// Which filters list `doc`, and whether its stored bytes were cut at a NUL
    /// ([`crate::membership`]).
    ///
    /// # Panics
    ///
    /// If `doc` is not less than [`Segment::len`].
    pub fn flags(&self, doc: u32) -> u8 {
        // Checked when the segment was opened.
        u8::try_from(u32_at(self.doc(doc), 32)).unwrap_or(0)
    }

    /// The root `doc` was found under and its path relative to that root.
    ///
    /// # Panics
    ///
    /// If `doc` is not less than [`Segment::len`].
    pub fn path(&self, doc: u32) -> (usize, PathBuf) {
        let d = self.doc(doc);
        let (path_offset, path_len, root) = (u32_at(d, 12), u32_at(d, 16), u32_at(d, 20));
        let start = path_offset as usize;
        let bytes = &self.paths[start..start + (path_len & !UTF16_PATH) as usize];
        let path = format::decode_path(bytes, path_len & UTF16_PATH != 0).unwrap_or_default();
        (root as usize, path)
    }

    /// Where `doc`'s bytes are.
    ///
    /// # Panics
    ///
    /// If `doc` is not less than [`Segment::len`].
    pub fn location(&self, doc: u32) -> Location {
        let d = self.doc(doc);
        match u32_at(d, 0) {
            NO_BLOCK => Location::Empty,
            EXTERNAL_BLOCK => Location::External,
            block => Location::Block {
                block,
                offset: u32_at(d, 4),
                len: u32_at(d, 8),
            },
        }
    }

    /// The number of files containing trigram `key`, without reading its postings.
    pub fn count(&self, key: u32) -> u32 {
        self.find(key)
            .map_or(0, |i| u32_at(&self.dictionary, i * DICT_LEN + 4))
    }

    /// The files containing trigram `key`, in increasing order.
    pub fn postings(&self, key: u32) -> Result<Vec<u32>, IndexError> {
        let Some(i) = self.find(key) else {
            return Ok(Vec::new());
        };
        let entry = &self.dictionary[i * DICT_LEN..];
        let count = u32_at(entry, 4) as usize;
        let start = u64_at(entry, 8);
        let end = if (i + 1) * DICT_LEN < self.dictionary.len() {
            u64_at(&self.dictionary, (i + 1) * DICT_LEN + 8)
        } else {
            self.postings.1
        };
        let corrupt = || IndexError::Corrupt {
            path: self.path.clone(),
            reason: "bad postings",
        };
        let len = usize::try_from(end - start).map_err(|_| corrupt())?;
        let mut bytes = vec![0u8; len];
        read_at(&self.file, &mut bytes, self.postings.0 + start).map_err(|source| {
            IndexError::Io {
                path: self.path.clone(),
                source,
            }
        })?;
        let mut ids = Vec::with_capacity(count);
        let mut pos = 0;
        let mut previous = 0u32;
        for n in 0..count {
            let delta = format::get_varint(&bytes, &mut pos).ok_or_else(corrupt)?;
            if n > 0 && delta == 0 {
                return Err(corrupt());
            }
            previous = previous.checked_add(delta).ok_or_else(corrupt)?;
            ids.push(previous);
        }
        if pos != bytes.len() || previous >= self.len() {
            return Err(corrupt());
        }
        Ok(ids)
    }

    /// A reader for store blocks. Each worker thread needs its own.
    pub fn block_reader(&self) -> Result<BlockReader<'_>, IndexError> {
        Ok(BlockReader {
            segment: self,
            decompressor: zstd::bulk::Decompressor::new().map_err(|source| IndexError::Io {
                path: self.path.clone(),
                source,
            })?,
            compressed: Vec::new(),
            raw: Vec::new(),
        })
    }

    fn entries(&self, (offset, len): (u64, u64)) -> Result<Vec<Entry>, IndexError> {
        let corrupt = || IndexError::Corrupt {
            path: self.path.clone(),
            reason: "bad directory or control entries",
        };
        let mut bytes = vec![0u8; usize::try_from(len).map_err(|_| corrupt())?];
        read_at(&self.file, &mut bytes, offset).map_err(|source| IndexError::Io {
            path: self.path.clone(),
            source,
        })?;
        format::parse_entries(&bytes, self.roots).ok_or_else(corrupt)
    }

    fn doc(&self, doc: u32) -> &[u8] {
        let at = doc as usize * DOC_LEN;
        &self.docs[at..at + DOC_LEN]
    }

    fn find(&self, key: u32) -> Option<usize> {
        let n = self.dictionary.len() / DICT_LEN;
        let (mut lo, mut hi) = (0, n);
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            match u32_at(&self.dictionary, mid * DICT_LEN).cmp(&key) {
                std::cmp::Ordering::Less => lo = mid + 1,
                std::cmp::Ordering::Greater => hi = mid,
                std::cmp::Ordering::Equal => return Some(mid),
            }
        }
        None
    }
}

/// Reads and decompresses store blocks, reusing its buffers.
pub struct BlockReader<'a> {
    segment: &'a Segment,
    decompressor: zstd::bulk::Decompressor<'static>,
    compressed: Vec<u8>,
    raw: Vec<u8>,
}

impl BlockReader<'_> {
    /// The uncompressed bytes of block `block`.
    pub fn read(&mut self, block: u32) -> Result<&[u8], IndexError> {
        let segment = self.segment;
        let corrupt = || IndexError::Corrupt {
            path: segment.path.clone(),
            reason: "bad store block",
        };
        let at = block as usize * BLOCK_LEN;
        let entry = segment.blocks.get(at..at + BLOCK_LEN).ok_or_else(corrupt)?;
        let (offset, compressed, raw) = (u64_at(entry, 0), u32_at(entry, 8), u32_at(entry, 12));
        self.compressed.resize(compressed as usize, 0);
        read_at(
            &segment.file,
            &mut self.compressed,
            segment.store.0 + offset,
        )
        .map_err(|source| IndexError::Io {
            path: segment.path.clone(),
            source,
        })?;
        self.raw.resize(raw as usize, 0);
        let n = self
            .decompressor
            .decompress_to_buffer(&self.compressed, &mut self.raw[..])
            .map_err(|_| corrupt())?;
        if n != raw as usize {
            return Err(corrupt());
        }
        Ok(&self.raw)
    }
}

/// Sections read whole at open are smaller than this.
const MAX_EAGER_SECTION: u64 = 1 << 32;

/// Reads the trailer and checks that every section lies between header and trailer.
fn section_ranges(file: &File, file_bytes: u64) -> Result<[(u64, u64); SECTIONS], &'static str> {
    let mut trailer = [0u8; TRAILER_LEN];
    let data_end = file_bytes - TRAILER_LEN as u64;
    read_at(file, &mut trailer, data_end).map_err(|_| "unreadable trailer")?;
    if &trailer[SECTIONS * 16..] != END_MAGIC {
        return Err("incomplete");
    }
    let mut ranges = [(0u64, 0u64); SECTIONS];
    for (i, range) in ranges.iter_mut().enumerate() {
        let (offset, len) = (u64_at(&trailer, i * 16), u64_at(&trailer, i * 16 + 8));
        if offset < HEADER_LEN as u64 || offset.checked_add(len).is_none_or(|e| e > data_end) {
            return Err("section out of bounds");
        }
        if !section::is_lazy(i) && len > MAX_EAGER_SECTION {
            return Err("section too big");
        }
        *range = (offset, len);
    }
    Ok(ranges)
}

/// Checks the block table against the store; returns each block's uncompressed size.
fn check_blocks(blocks: &[u8], store_len: u64) -> Result<Vec<u32>, &'static str> {
    let (entries, _) = blocks.as_chunks::<BLOCK_LEN>();
    entries
        .iter()
        .map(|b| {
            let (offset, compressed, raw) = (u64_at(b, 0), u32_at(b, 8), u32_at(b, 12));
            if offset + u64::from(compressed) > store_len || u64::from(raw) > MAX_STORED {
                Err("block out of bounds")
            } else {
                Ok(raw)
            }
        })
        .collect()
}

/// Checks that every file entry points inside its block, at a valid root and a decodable path.
fn check_docs(
    docs: &[u8],
    paths: &[u8],
    block_raw: &[u32],
    roots: u32,
) -> Result<(), &'static str> {
    let (entries, _) = docs.as_chunks::<DOC_LEN>();
    for d in entries {
        let [block, offset, len, path_offset, path_len, root] =
            [0, 4, 8, 12, 16, 20].map(|at| u32_at(d, at));
        let in_block = match block {
            NO_BLOCK | EXTERNAL_BLOCK => len == 0,
            b => block_raw
                .get(b as usize)
                .is_some_and(|&raw| u64::from(offset) + u64::from(len) <= u64::from(raw)),
        };
        if !in_block || root >= roots || u32_at(d, 32) > u32::from(u8::MAX) {
            return Err("file entry out of bounds");
        }
        let start = path_offset as usize;
        let end = start + (path_len & !UTF16_PATH) as usize;
        if paths
            .get(start..end)
            .and_then(|p| format::decode_path(p, path_len & UTF16_PATH != 0))
            .is_none()
        {
            return Err("bad path");
        }
    }
    Ok(())
}

/// Checks that dictionary keys increase and their postings lie inside the postings section.
fn check_dictionary(
    dictionary: &[u8],
    doc_count: u32,
    postings_len: u64,
) -> Result<(), &'static str> {
    let (entries, _) = dictionary.as_chunks::<DICT_LEN>();
    let mut previous = None;
    for e in entries {
        let (key, count, offset) = (u32_at(e, 0), u32_at(e, 4), u64_at(e, 8));
        let in_order = previous.is_none_or(|(k, o)| key > k && offset >= o);
        if !in_order || key >= 1 << 24 || count == 0 || count > doc_count {
            return Err("dictionary out of order");
        }
        if offset > postings_len {
            return Err("postings out of bounds");
        }
        previous = Some((key, offset));
    }
    Ok(())
}

fn parse_errors(mut bytes: &[u8], count: u32, roots: u32) -> Option<Vec<StoredError>> {
    use format::take;
    let mut errors = Vec::new();
    for _ in 0..count {
        let root = u32_at(take(&mut bytes, 4)?, 0);
        let flags = u8::try_from(u32_at(take(&mut bytes, 4)?, 0)).ok()?;
        let path_len = u32_at(take(&mut bytes, 4)?, 0);
        let path = format::decode_path(
            take(&mut bytes, (path_len & !UTF16_PATH) as usize)?,
            path_len & UTF16_PATH != 0,
        )?;
        let message_len = u32_at(take(&mut bytes, 4)?, 0) as usize;
        let message = std::str::from_utf8(take(&mut bytes, message_len)?)
            .ok()?
            .to_owned();
        let root = (root != u32::MAX).then_some(root);
        if root.is_some_and(|r| r >= roots) {
            return None;
        }
        errors.push(StoredError {
            root,
            path,
            message,
            flags,
        });
    }
    bytes.is_empty().then_some(errors)
}

/// Fills `buf` from `file` at `offset`.
pub(crate) fn read_at(file: &File, mut buf: &mut [u8], mut offset: u64) -> io::Result<()> {
    #[cfg(windows)]
    {
        use std::os::windows::fs::FileExt;
        while !buf.is_empty() {
            let n = file.seek_read(buf, offset)?;
            if n == 0 {
                return Err(io::ErrorKind::UnexpectedEof.into());
            }
            buf = &mut buf[n..];
            offset += n as u64;
        }
        Ok(())
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileExt;
        file.read_exact_at(buf, offset)
    }
}
