//! Build a [`NameIndex`] by walking directories in parallel.
//!
//! Used when the NTFS master file table can't be read (no admin rights, ReFS/FAT, network
//! drives), and to read directories that appeared since a snapshot was saved. It covers
//! every file, hidden and gitignored ones included, like the MFT does. It doesn't follow
//! symbolic links or junctions.
//!
//! On Windows each entry's id is its NTFS file reference number, read in the same call
//! that lists the directory. That is what lets a saved index be matched against the USN
//! change journal later. File systems without reference numbers get counter ids.
//!
//! Workers share a stack of directories to read. Each worker reads one directory at a time,
//! keeps the entries in a local list, and pushes subdirectories back onto the stack. The
//! lists are merged into the index at the end, so the index is never written by two threads.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Condvar, Mutex, PoisonError};

use crate::{Child, FileId, Kind, NameIndex};

/// What the walk saw.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct WalkStats {
    /// Entries added.
    pub entries: u64,
    /// Directories that could not be read (permission denied, deleted mid-walk).
    pub unreadable_dirs: u64,
}

/// Walks `root` and returns an index of everything under it, using every logical CPU.
pub fn walk(root: &Path) -> io::Result<(NameIndex, WalkStats)> {
    let (root_id, _) = Lister::default().list(root)?; // also fails early on a missing root
    let mut index = NameIndex::new(root_id, root);
    let stats = walk_into(&mut index, vec![(root.to_path_buf(), root_id)]);
    Ok((index, stats))
}

/// Reads each `(path, id)` directory and everything below it into `index`.
///
/// A subdirectory the index already knows is a directory that was moved here: it is
/// re-parented, but not read again, because its subtree is already indexed.
pub fn walk_into(index: &mut NameIndex, starts: Vec<(PathBuf, FileId)>) -> WalkStats {
    if starts.is_empty() {
        return WalkStats::default();
    }
    let threads = std::thread::available_parallelism().map_or(1, std::num::NonZero::get);
    let shared = Shared {
        work: Mutex::new(Work {
            dirs: starts,
            busy: 0,
        }),
        changed: Condvar::new(),
        unreadable: AtomicU64::new(0),
        known: index,
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

    let mut totals = WalkStats {
        entries: 0,
        unreadable_dirs: shared.unreadable.into_inner(),
    };
    for entry in batches.into_iter().flatten() {
        index.insert(entry.id, entry.parent, &entry.name, entry.kind);
        totals.entries += 1;
    }
    totals
}

/// Lists one directory: its own id and its children. Reuses buffers between calls.
#[derive(Default)]
pub(crate) struct Lister {
    #[cfg(windows)]
    buf: mtseek_ntfs::DirBuffer,
    #[cfg(windows)]
    raw: Vec<mtseek_ntfs::DirEntry>,
    children: Vec<Child>,
}

impl Lister {
    /// Reads `dir`. The children are in `self.children` until the next call.
    #[cfg(windows)]
    pub(crate) fn list(&mut self, dir: &Path) -> io::Result<(FileId, &[Child])> {
        let handle = mtseek_ntfs::Directory::open(dir)?;
        self.raw.clear();
        handle.read(&mut self.buf, &mut self.raw)?;
        self.children.clear();
        self.children.extend(self.raw.drain(..).map(|e| Child {
            id: real_or_counter(e.file_ref),
            kind: if e.is_dir() { Kind::Dir } else { Kind::File },
            name: e.name,
        }));
        Ok((real_or_counter(handle.file_ref()?), &self.children))
    }

    /// Reads `dir`. The children are in `self.children` until the next call.
    #[cfg(not(windows))]
    pub(crate) fn list(&mut self, dir: &Path) -> io::Result<(FileId, &[Child])> {
        self.children.clear();
        for entry in std::fs::read_dir(dir)?.flatten() {
            // `file_type()` does not follow links, so symlinks are leaves.
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            self.children.push(Child {
                id: real_or_counter(0),
                name: entry.file_name().to_string_lossy().into_owned(),
                kind: if file_type.is_dir() {
                    Kind::Dir
                } else {
                    Kind::File
                },
            });
        }
        Ok((real_or_counter(0), &self.children))
    }
}

/// File systems without reference numbers report 0; those entries get ids from a counter
/// in the top half of the id space, which NTFS sequence numbers there never reach.
fn real_or_counter(file_ref: u64) -> FileId {
    static NEXT: AtomicU64 = AtomicU64::new(1 << 63);
    if file_ref == 0 {
        NEXT.fetch_add(1, Ordering::Relaxed)
    } else {
        file_ref
    }
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

struct Shared<'a> {
    work: Mutex<Work>,
    changed: Condvar,
    unreadable: AtomicU64,
    /// The index being added to, read-only until the merge.
    known: &'a NameIndex,
}

impl Shared<'_> {
    fn run(&self) -> Vec<Entry> {
        let mut found = Vec::new();
        let mut subdirs = Vec::new();
        let mut lister = Lister::default();
        while let Some((dir, dir_id)) = self.take() {
            self.read(&mut lister, &dir, dir_id, &mut found, &mut subdirs);
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
        lister: &mut Lister,
        dir: &Path,
        dir_id: FileId,
        found: &mut Vec<Entry>,
        subdirs: &mut Vec<(PathBuf, FileId)>,
    ) {
        let Ok((_, children)) = lister.list(dir) else {
            self.unreadable.fetch_add(1, Ordering::Relaxed);
            return;
        };
        for child in children {
            if child.kind == Kind::Dir && !self.known.has_dir(child.id) {
                subdirs.push((dir.join(&child.name), child.id));
            }
            found.push(Entry {
                id: child.id,
                parent: dir_id,
                name: child.name.clone(),
                kind: child.kind,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::NameQuery;
    use mtseek_core::CaseMode;
    use std::fs;

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
                root.join("src").join(".hidden").join("report.md"),
                root.join("target").join("report.txt")
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

    #[cfg(windows)]
    #[test]
    fn ids_are_file_reference_numbers_and_junctions_are_leaves() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("real/inner")).unwrap();
        fs::write(root.join("real/inner/x.txt"), "").unwrap();
        // Junctions need no admin rights. If following them, x.txt would appear twice.
        let made = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(root.join("link"))
            .arg(root.join("real"))
            .output()
            .unwrap();
        assert!(made.status.success(), "mklink /J failed");

        let (index, _) = walk(root).unwrap();
        assert_eq!(index.root_id(), mtseek_ntfs::file_ref(root).unwrap());
        let inner = mtseek_ntfs::file_ref(&root.join("real/inner")).unwrap();
        assert_eq!(
            index.dir_path(inner).unwrap(),
            root.join("real").join("inner")
        );
        let found = index.search(&NameQuery::new("x.txt", CaseMode::Smart), None, 0);
        assert_eq!(found.len(), 1, "{found:?}");
        let link = index.search(&NameQuery::new("link", CaseMode::Smart), None, 0);
        assert_eq!(link, [root.join("link")], "the junction itself is listed");
    }

    #[test]
    fn missing_root_is_an_error() {
        assert!(walk(Path::new(r"Z:\definitely\not\here")).is_err());
    }
}
