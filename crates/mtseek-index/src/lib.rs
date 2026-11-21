//! Content index segments (ADR 0001, 0002, 0010).
//!
//! A segment holds, for every file of a scope:
//!
//! * its **stored bytes**, the raw file cut where ripgrep's searcher would stop reading,
//!   compressed with zstd in ~64 KiB blocks ([`text::stored_len`])
//! * its distinct **trigrams** (three ASCII-folded bytes of the text the matcher sees),
//!   as postings lists of file ids
//!
//! [`build`] makes one from a scope. [`Segment`] opens it to look up postings and read
//! blocks. Planning and verifying queries is `mtseek-query`'s job.

mod build;
mod format;
mod live;
mod ntfs;
mod read;
pub mod text;
pub mod trigram;
mod write;

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

pub use build::{build, coverage, read_file, scope_key};
pub use live::{CatchUp, Index, Persist, delta_path};
pub use read::{BlockReader, Location, Segment};
pub use write::{
    Content, Doc, SegmentInfo, StoredError, WriteStats, segment_bytes, write_bytes, write_segment,
};

/// A directory or control file the index tracks by file reference number (ADR 0011).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Entry {
    /// Its NTFS file reference number.
    pub frn: u64,
    /// The root it is under.
    pub root: u32,
    /// Its path relative to that root (empty for the root itself).
    pub path: PathBuf,
    /// For a directory: the filters whose walk enters it ([`membership`]). 0 for control
    /// files.
    pub flags: u8,
}

/// Which walks list an entry (ADR 0012).
///
/// An index covers a root at one filter: its *coverage filter*. Every file, directory and
/// error in it records which of the four combinations of "honour ignore files" and "include
/// hidden entries" would list it too, as long as that combination lists no more than the
/// coverage filter does. A search with a narrower filter is then answered from the same index,
/// exactly. Binary mode (`-uuu`) and following links are not part of this: binary mode reads
/// [`membership::TRUNCATED`] files from disk, and a search that follows links never uses the
/// index.
pub mod membership {
    use mtseek_core::Filter;

    /// Ignore files honoured, hidden entries skipped: ripgrep's defaults.
    pub const DEFAULT: u8 = 1;
    /// Ignore files not honoured, hidden entries skipped (`-u`, `--no-ignore`).
    pub const NO_IGNORE: u8 = 2;
    /// Ignore files honoured, hidden entries included (`--hidden`).
    pub const HIDDEN: u8 = 4;
    /// Neither (`-uu`).
    pub const ALL: u8 = 8;
    /// All four filter bits.
    pub const FILTERS: u8 = DEFAULT | NO_IGNORE | HIDDEN | ALL;
    /// Files only: the stored bytes end at a NUL (ripgrep stops reading there unless it
    /// searches binary files). A binary-mode search reads the file from disk.
    pub const TRUNCATED: u8 = 16;

    /// The bit of `filter`'s walk.
    pub fn bit(filter: &Filter) -> u8 {
        match (filter.respect_ignore_files, filter.include_hidden) {
            (true, false) => DEFAULT,
            (false, false) => NO_IGNORE,
            (true, true) => HIDDEN,
            (false, true) => ALL,
        }
    }

    /// The bits of every walk that lists no more than `filter`'s walk: the filters an index
    /// built with `filter` can answer.
    pub fn covered(filter: &Filter) -> u8 {
        let mut bits = DEFAULT;
        if !filter.respect_ignore_files {
            bits |= NO_IGNORE;
        }
        if filter.include_hidden {
            bits |= HIDDEN;
        }
        if !filter.respect_ignore_files && filter.include_hidden {
            bits |= ALL;
        }
        bits
    }

    /// The filter of walk `bit`, with binary files skipped and links not followed.
    pub fn filter(bit: u8) -> Filter {
        let mut f = Filter::unrestricted(0);
        f.respect_ignore_files = bit & (DEFAULT | HIDDEN) != 0;
        f.include_hidden = bit & (HIDDEN | ALL) != 0;
        f
    }
}

/// Where a segment is up to date in one drive's change journal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Checkpoint {
    /// The drive letter.
    pub letter: char,
    /// The journal's identity; a recreated journal has another.
    pub journal_id: u64,
    /// The next change the segment hasn't seen.
    pub usn: i64,
}

/// A segment's identity and freshness.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Meta {
    /// Unique per written segment.
    pub build_id: u64,
    /// For a delta: the `build_id` of the base it applies to. 0 for a base.
    pub base_id: u64,
    /// One per drive of the scope's roots. Empty if the journal isn't available.
    pub checkpoints: Vec<Checkpoint>,
}

/// Files bigger than this aren't stored; every query reads them from disk instead.
pub const MAX_STORED: u64 = 256 << 20;

/// The block of a file that is read from disk at query time.
pub(crate) const EXTERNAL_BLOCK: u32 = u32::MAX - 1;

/// Why a segment couldn't be written or read.
#[derive(Debug, thiserror::Error)]
pub enum IndexError {
    /// Reading or writing the segment file failed.
    #[error("{}: {source}", path.display())]
    Io {
        /// The segment file.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: std::io::Error,
    },
    /// The file isn't a usable segment: damaged, truncated, or another format version.
    #[error("{}: not a usable content index ({reason})", path.display())]
    Corrupt {
        /// The segment file.
        path: PathBuf,
        /// What was wrong.
        reason: &'static str,
    },
}

impl From<IndexError> for mtseek_core::Error {
    fn from(err: IndexError) -> Self {
        match err {
            IndexError::Io { path, source } => Self::Io { path, source },
            IndexError::Corrupt { path, reason } => Self::Io {
                path,
                source: std::io::Error::new(std::io::ErrorKind::InvalidData, reason),
            },
        }
    }
}

/// Maps `f` over `items` on up to `threads` threads, keeping the order.
pub(crate) fn parallel_map<T: Sync, R: Send>(
    items: &[T],
    threads: usize,
    f: impl Fn(&T) -> R + Sync,
) -> Vec<R> {
    let threads = threads.clamp(1, items.len().max(1));
    if threads == 1 {
        return items.iter().map(f).collect();
    }
    let next = AtomicUsize::new(0);
    let parts: Vec<Vec<(usize, R)>> = std::thread::scope(|scope| {
        let workers: Vec<_> = (0..threads)
            .map(|_| {
                scope.spawn(|| {
                    let mut out = Vec::new();
                    loop {
                        let i = next.fetch_add(1, Ordering::Relaxed);
                        let Some(item) = items.get(i) else { break };
                        out.push((i, f(item)));
                    }
                    out
                })
            })
            .collect();
        workers
            .into_iter()
            .map(|w| w.join().unwrap_or_else(|p| std::panic::resume_unwind(p)))
            .collect()
    });
    let mut slots: Vec<Option<R>> = std::iter::repeat_with(|| None).take(items.len()).collect();
    for (i, r) in parts.into_iter().flatten() {
        slots[i] = Some(r);
    }
    slots
        .into_iter()
        .map(|r| r.expect("every item is mapped once"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use std::path::Path;

    fn doc(root: u32, path: &str, bytes: &[u8]) -> Doc {
        Doc {
            root,
            path: PathBuf::from(path),
            frn: bytes.len() as u64 + 1,
            flags: membership::DEFAULT,
            content: Content::Stored(bytes.to_vec()),
        }
    }

    fn write(path: &Path, roots: u32, docs: &mut [Doc], errors: &[StoredError]) -> WriteStats {
        let meta = Meta::default();
        let info = SegmentInfo {
            key: "key",
            roots,
            errors,
            dirs: &[],
            controls: &[],
            tombstones: &[],
            meta: &meta,
        };
        write_segment(path, &info, docs, 4).unwrap()
    }

    /// Every stored file's bytes, read back through the segment.
    fn contents(segment: &Segment) -> Vec<(usize, PathBuf, Option<Vec<u8>>)> {
        let mut reader = segment.block_reader().unwrap();
        (0..segment.len())
            .map(|d| {
                let (root, path) = segment.path(d);
                let bytes = match segment.location(d) {
                    Location::Empty => Some(Vec::new()),
                    Location::External => None,
                    Location::Block { block, offset, len } => {
                        let raw = reader.read(block).unwrap();
                        Some(raw[offset as usize..(offset + len) as usize].to_vec())
                    }
                };
                (root, path, bytes)
            })
            .collect()
    }

    #[test]
    fn a_written_segment_reads_back() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("seg");
        let big = vec![b'x'; 200_000];
        let mut docs = vec![
            doc(1, "b.txt", b"second root"),
            doc(0, "z.txt", b"Hello World"),
            doc(0, "a.txt", b"hello there"),
            doc(0, "empty", b""),
            doc(0, "big", &big),
            Doc {
                root: 0,
                path: PathBuf::from("huge"),
                frn: 99,
                flags: membership::DEFAULT | membership::ALL,
                content: Content::External,
            },
        ];
        let errors = vec![StoredError {
            root: Some(0),
            path: PathBuf::from("locked"),
            message: "Access is denied. (os error 5)".into(),
            flags: membership::NO_IGNORE,
        }];
        let stats = write(&path, 2, &mut docs, &errors);
        assert_eq!((stats.files, stats.external), (6, 1));

        let segment = Segment::open(&path).unwrap();
        assert_eq!(segment.key(), "key");
        assert_eq!(segment.roots(), 2);
        assert_eq!(segment.errors(), &errors[..]);
        let got = contents(&segment);
        let expected: Vec<(usize, PathBuf, Option<Vec<u8>>)> = vec![
            (0, "a.txt".into(), Some(b"hello there".to_vec())),
            (0, "big".into(), Some(big.clone())),
            (0, "empty".into(), Some(Vec::new())),
            (0, "huge".into(), None),
            (0, "z.txt".into(), Some(b"Hello World".to_vec())),
            (1, "b.txt".into(), Some(b"second root".to_vec())),
        ];
        assert_eq!(got, expected);

        // "hel" is in a.txt and z.txt (case-folded), "sec" only in b.txt.
        assert_eq!(
            segment.postings(trigram::key(b'H', b'E', b'L')).unwrap(),
            vec![0, 4]
        );
        assert_eq!(segment.count(trigram::key(b'h', b'e', b'l')), 2);
        assert_eq!(
            segment.postings(trigram::key(b's', b'e', b'c')).unwrap(),
            vec![5]
        );
        assert_eq!(
            segment.postings(trigram::key(b'q', b'q', b'q')).unwrap(),
            []
        );
    }

    #[test]
    fn directories_controls_tombstones_and_meta_read_back() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("seg");
        let dirs = vec![
            Entry {
                frn: 1,
                root: 0,
                path: PathBuf::new(),
                flags: membership::FILTERS,
            },
            Entry {
                frn: 2,
                root: 0,
                path: PathBuf::from("src"),
                flags: membership::ALL,
            },
        ];
        let controls = vec![Entry {
            frn: 3,
            root: 0,
            path: PathBuf::from(".gitignore"),
            flags: 0,
        }];
        let meta = Meta {
            build_id: 10,
            base_id: 20,
            checkpoints: vec![Checkpoint {
                letter: 'C',
                journal_id: 5,
                usn: 6,
            }],
        };
        let info = SegmentInfo {
            key: "key",
            roots: 1,
            errors: &[],
            dirs: &dirs,
            controls: &controls,
            tombstones: &[0, 4, 9],
            meta: &meta,
        };
        let mut docs = vec![doc(0, "src/a.rs", b"fn main() {}")];
        write_segment(&path, &info, &mut docs, 1).unwrap();
        let segment = Segment::open(&path).unwrap();
        assert_eq!(segment.dirs().unwrap(), dirs);
        assert_eq!(segment.controls().unwrap(), controls);
        assert_eq!(segment.tombstones(), [0, 4, 9]);
        assert_eq!(segment.meta(), &meta);
        assert_eq!(segment.frn(0), 13);
    }

    #[test]
    fn damaged_segments_are_errors_not_panics() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("seg");
        let mut docs = vec![doc(0, "a", b"some text here"), doc(0, "b", b"more text")];
        write(&path, 1, &mut docs, &[]);
        let good = std::fs::read(&path).unwrap();
        // Cut anywhere: always an error.
        for len in (0..good.len()).step_by(7) {
            std::fs::write(&path, &good[..len]).unwrap();
            assert!(Segment::open(&path).is_err(), "truncated to {len}");
        }
        assert!(Segment::open(Path::new("does not exist")).is_err());
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(64))]

        /// Flipping any byte gives an error at open, an error on read, or (when the byte is
        /// in a place nothing checks, like compressed data that still decodes) a segment
        /// that still works. Never a panic.
        #[test]
        fn corrupt_bytes_never_panic(at in any::<prop::sample::Index>(), value in any::<u8>()) {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("seg");
            let mut docs = vec![
                doc(0, "a", b"alpha beta gamma"),
                doc(0, "b", b"beta delta"),
                doc(0, "c", &[b'y'; 70_000]),
            ];
            write(&path, 1, &mut docs, &[]);
            let mut bytes = std::fs::read(&path).unwrap();
            let i = at.index(bytes.len());
            bytes[i] = value;
            std::fs::write(&path, &bytes).unwrap();
            if let Ok(segment) = Segment::open(&path) {
                let _ = (segment.dirs(), segment.controls());
                for d in 0..segment.len() {
                    let _ = segment.path(d);
                    if let Location::Block { block, .. } = segment.location(d) {
                        let _ = segment.block_reader().unwrap().read(block);
                    }
                }
                for key in [trigram::key(b'b', b'e', b't'), trigram::key(b'y', b'y', b'y')] {
                    let _ = segment.postings(key);
                }
            }
        }

        #[test]
        fn parallel_map_keeps_order(items in proptest::collection::vec(any::<u32>(), 0..200), threads in 1usize..9) {
            let mapped = parallel_map(&items, threads, |x| u64::from(*x) * 3);
            let expected: Vec<u64> = items.iter().map(|x| u64::from(*x) * 3).collect();
            prop_assert_eq!(mapped, expected);
        }
    }
}
