//! Portable fallback: build a [`NameIndex`] by walking directories in parallel.
//!
//! Used when the NTFS master file table can't be read (no admin rights, ReFS/FAT, network
//! drives). It covers every file, hidden and gitignored ones included, like the MFT does.
//! It doesn't follow symbolic links or junctions.
//!
//! Workers share a stack of directories to read. Each worker reads one directory at a time,
//! numbers its entries from a shared counter, keeps them in a local list, and pushes
//! subdirectories back onto the stack. The lists are merged into the index at the end, so
//! the index itself is never shared between threads.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Condvar, Mutex, PoisonError};

use crate::{FileId, Kind, NameIndex};

/// What the walk saw.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct WalkStats {
    /// Entries added.
    pub entries: u64,
    /// Directories that could not be read (permission denied, deleted mid-walk).
    pub unreadable_dirs: u64,
}

/// Walks `root` and returns an index of everything under it, using every logical CPU.
/// The root gets id 0; other ids are unique but depend on thread timing.
pub fn walk(root: &Path) -> io::Result<(NameIndex, WalkStats)> {
    fs::metadata(root)?; // fail early on a missing root
    let threads = std::thread::available_parallelism().map_or(1, std::num::NonZero::get);
    let shared = Shared {
        work: Mutex::new(Work {
            dirs: vec![(root.to_path_buf(), 0)],
            busy: 0,
        }),
        changed: Condvar::new(),
        next_id: AtomicU64::new(1),
        unreadable: AtomicU64::new(0),
    };

    let batches: Vec<Vec<Entry>> = std::thread::scope(|scope| {
        let workers: Vec<_> = (0..threads).map(|_| scope.spawn(|| shared.run())).collect();
        workers
            .into_iter()
            .map(|w| {
                w.join()
                    .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
            })
            .collect()
    });

    let mut index = NameIndex::new(0, root);
    let mut stats = WalkStats {
        entries: 0,
        unreadable_dirs: shared.unreadable.into_inner(),
    };
    for entry in batches.into_iter().flatten() {
        index.upsert(entry.id, entry.parent, &entry.name, entry.kind);
        stats.entries += 1;
    }
    Ok((index, stats))
}

struct Entry {
    id: FileId,
    parent: FileId,
    name: String,
    kind: Kind,
}

struct Work {
    /// Directories waiting to be read, with their ids.
    dirs: Vec<(PathBuf, FileId)>,
    /// Workers currently reading a directory (and so possibly about to push more work).
    busy: usize,
}

struct Shared {
    work: Mutex<Work>,
    changed: Condvar,
    next_id: AtomicU64,
    unreadable: AtomicU64,
}

impl Shared {
    fn run(&self) -> Vec<Entry> {
        let mut found = Vec::new();
        let mut subdirs = Vec::new();
        while let Some((dir, dir_id)) = self.take() {
            self.read(&dir, dir_id, &mut found, &mut subdirs);
            let mut work = self.work.lock().unwrap_or_else(PoisonError::into_inner);
            work.dirs.append(&mut subdirs);
            work.busy -= 1;
            self.changed.notify_all();
        }
        found
    }

    /// Next directory to read, or `None` once nothing is queued and nobody is busy.
    fn take(&self) -> Option<(PathBuf, FileId)> {
        let mut work = self.work.lock().unwrap_or_else(PoisonError::into_inner);
        loop {
            if let Some(job) = work.dirs.pop() {
                work.busy += 1;
                return Some(job);
            }
            if work.busy == 0 {
                return None;
            }
            work = self
                .changed
                .wait(work)
                .unwrap_or_else(PoisonError::into_inner);
        }
    }

    fn read(
        &self,
        dir: &Path,
        dir_id: FileId,
        found: &mut Vec<Entry>,
        subdirs: &mut Vec<(PathBuf, FileId)>,
    ) {
        let Ok(entries) = fs::read_dir(dir) else {
            self.unreadable.fetch_add(1, Ordering::Relaxed);
            return;
        };
        for entry in entries.flatten() {
            // `file_type()` does not follow links, so junctions and symlinks are leaves.
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            let id = self.next_id.fetch_add(1, Ordering::Relaxed);
            let kind = if file_type.is_dir() {
                Kind::Dir
            } else {
                Kind::File
            };
            if file_type.is_dir() {
                subdirs.push((entry.path(), id));
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            found.push(Entry {
                id,
                parent: dir_id,
                name,
                kind,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::NameQuery;
    use mtseek_core::CaseMode;

    #[test]
    fn indexes_everything_including_hidden_and_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("src/.hidden")).unwrap();
        fs::write(root.join(".gitignore"), "target/\n").unwrap();
        fs::create_dir_all(root.join("target")).unwrap();
        fs::write(root.join("target/report.txt"), "").unwrap();
        fs::write(root.join("src/.hidden/report.md"), "").unwrap();
        fs::write(root.join("src/main.rs"), "").unwrap();

        let (index, stats) = walk(root).unwrap();
        assert_eq!(stats.entries, 7);
        let found = index.search(&NameQuery::new("report", CaseMode::Smart), None, 0);
        assert_eq!(
            found,
            vec![
                root.join("src/.hidden/report.md"),
                root.join("target/report.txt")
            ]
        );
    }

    #[test]
    fn deep_and_wide_trees_are_complete() {
        let dir = tempfile::tempdir().unwrap();
        let mut deep = dir.path().to_path_buf();
        for level in 0..30 {
            deep.push(format!("d{level}"));
        }
        fs::create_dir_all(&deep).unwrap();
        fs::write(deep.join("bottom.txt"), "").unwrap();
        for i in 0..200 {
            fs::write(dir.path().join(format!("wide{i}.txt")), "").unwrap();
        }
        let (index, stats) = walk(dir.path()).unwrap();
        assert_eq!(stats.entries, 30 + 1 + 200);
        let found = index.search(&NameQuery::new("bottom", CaseMode::Smart), None, 0);
        assert_eq!(found, vec![deep.join("bottom.txt")]);
    }

    #[test]
    fn missing_root_is_an_error() {
        assert!(walk(Path::new(r"Z:\definitely\not\here")).is_err());
    }
}
