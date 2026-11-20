//! The file-name index: every file and directory under a root, searchable by name.
//!
//! # Layout
//!
//! Names live in two flat byte arenas, each name prefixed by a NUL separator:
//! one in original case, one case-folded. A search is a single SIMD substring scan
//! (`memchr::memmem`) over the arena, split across threads. Each hit maps back to its entry
//! with a binary search over entry start offsets. That is the same idea that makes
//! Everything fast. There is no tree to walk and no per-entry allocation, and 2M names fit in
//! about 100 MB.
//!
//! Entries know only their parent's id, so a rename or move is one entry update. Paths are
//! built on demand, and only for results.
//!
//! # Sources and freshness
//!
//! An index is built from the NTFS master file table (Windows, admin) or by [`walk`]ing
//! directories (anyone). A [`Snapshot`] saves it with the change-journal position it
//! reflects, and `catch_up` (Windows, no admin needed) applies everything that changed
//! since, by re-listing only the directories the journal names.

#[cfg(windows)]
mod live;
mod query;
mod snapshot;
mod walk;

#[cfg(windows)]
pub use live::{CatchUp, MftBuild, MftMethod, build_from_mft, catch_up, checkpoint_now};
pub use query::NameQuery;
pub use snapshot::{Checkpoint, Origin, Snapshot};
pub use walk::{WalkStats, walk, walk_into};

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

/// A file's identity: its NTFS file reference number, or a counter on file systems without one.
pub type FileId = u64;

/// Whether an entry is a file or a directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// A file (or anything that isn't a directory).
    File,
    /// A directory.
    Dir,
}

/// One entry of a directory listing, for [`NameIndex::replace_children`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Child {
    /// The entry's id.
    pub id: FileId,
    /// Its file name.
    pub name: String,
    /// File or directory.
    pub kind: Kind,
}

/// What [`NameIndex::replace_children`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Replaced {
    /// Directories that were not in the index before. The caller must read their contents.
    pub new_dirs: Vec<FileId>,
    /// Whether any listed directory's contents differ from what the index held.
    pub changed: bool,
}

/// One child, as compared by [`NameIndex::replace_children`]: a file is its name, a
/// directory is its name and id (a different id is a different directory and subtree).
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Listed {
    parent: FileId,
    name: String,
    dir_id: Option<FileId>,
}

impl Listed {
    fn new(parent: FileId, name: &str, is_dir: bool, id: FileId) -> Self {
        Self {
            parent,
            name: name.to_owned(),
            dir_id: is_dir.then_some(id),
        }
    }
}

const DEAD: u8 = 1;
const DIR: u8 = 2;

#[derive(Debug, Clone, Copy)]
struct Slot {
    id: FileId,
    parent: FileId,
    /// Offset of the name in `names` (after its NUL separator).
    name: u32,
    name_len: u32,
    /// Offset of the folded name in `folded`.
    folded: u32,
    folded_len: u32,
    flags: u8,
}

/// The index. See the crate docs for the layout.
///
/// Directories are unique by id: inserting a directory id again moves or renames it, and its
/// children follow. Files are not keyed by id at all, so a file with several hard links
/// appears once per directory that links it, like `rg --files`. Files change only through
/// [`Self::replace_children`].
#[derive(Debug, Clone)]
pub struct NameIndex {
    root_id: FileId,
    root_path: PathBuf,
    slots: Vec<Slot>,
    /// Directory id -> slot. Files are not in here.
    dirs: HashMap<FileId, u32>,
    names: Vec<u8>,
    folded: Vec<u8>,
    live: usize,
}

impl NameIndex {
    /// An empty index for the tree rooted at `root_path`, whose own id is `root_id`.
    pub fn new(root_id: FileId, root_path: impl Into<PathBuf>) -> Self {
        Self {
            root_id,
            root_path: root_path.into(),
            slots: Vec::new(),
            dirs: HashMap::new(),
            names: Vec::new(),
            folded: Vec::new(),
            live: 0,
        }
    }

    /// Number of live entries.
    pub fn len(&self) -> usize {
        self.live
    }

    /// Whether the index has no live entries.
    pub fn is_empty(&self) -> bool {
        self.live == 0
    }

    /// The root's id.
    pub fn root_id(&self) -> FileId {
        self.root_id
    }

    /// The root's path.
    pub fn root_path(&self) -> &Path {
        &self.root_path
    }

    /// Bytes held by the name arenas and entry table. Useful for status displays.
    pub fn memory_bytes(&self) -> usize {
        self.names.capacity()
            + self.folded.capacity()
            + self.slots.capacity() * size_of::<Slot>()
            + self.dirs.capacity() * (size_of::<FileId>() + size_of::<u32>() + 1)
    }

    /// Adds an entry. A directory whose id is already present is moved and renamed instead.
    /// Entries for the root's own id are ignored: the root is implicit.
    pub fn insert(&mut self, id: FileId, parent: FileId, name: &str, kind: Kind) {
        if id == self.root_id {
            return;
        }
        let slot = len_u32(self.slots.len());
        if kind == Kind::Dir
            && let Some(old) = self.dirs.insert(id, slot)
        {
            self.kill_slot(old as usize);
        }
        let folded_name = name.to_lowercase();
        self.slots.push(Slot {
            id,
            parent,
            name: push_bytes(&mut self.names, name.as_bytes()),
            name_len: len_u32(name.len()),
            folded: push_bytes(&mut self.folded, folded_name.as_bytes()),
            folded_len: len_u32(folded_name.len()),
            flags: if kind == Kind::Dir { DIR } else { 0 },
        });
        self.live += 1;
    }

    /// Whether `id` is the root or a live directory.
    pub fn has_dir(&self, id: FileId) -> bool {
        id == self.root_id || self.dirs.contains_key(&id)
    }

    /// The full path of the root or a live directory, or `None` if it is unknown or
    /// detached from the root.
    pub fn dir_path(&self, id: FileId) -> Option<PathBuf> {
        if id == self.root_id {
            return Some(self.root_path.clone());
        }
        self.path_of(*self.dirs.get(&id)?, None)
    }

    /// Replaces the children of each listed directory with its fresh listing: what a
    /// directory re-read returns after the change journal says it changed.
    ///
    /// A directory that disappears from every listing is dropped with its whole subtree.
    /// One that moved between two listed directories keeps its subtree.
    pub fn replace_children(&mut self, listings: Vec<(FileId, Vec<Child>)>) -> Replaced {
        let listed: HashSet<FileId> = listings.iter().map(|(dir, _)| *dir).collect();
        // What the listed directories held, to tell a real change from a no-op (a file
        // created and deleted again, or replaced under the same name).
        let mut before = Vec::new();
        let mut dropped = Vec::new();
        for i in 0..self.slots.len() {
            let slot = self.slots[i];
            if slot.flags & DEAD == 0 && listed.contains(&slot.parent) {
                let is_dir = slot.flags & DIR != 0;
                if is_dir {
                    dropped.push(slot.id);
                }
                before.push(Listed::new(slot.parent, self.name(&slot), is_dir, slot.id));
                self.kill_slot(i);
            }
        }
        let dropped_set: HashSet<FileId> = dropped.iter().copied().collect();
        let mut after = Vec::with_capacity(before.len());
        let mut new_dirs = Vec::new();
        for (parent, children) in listings {
            for child in children {
                let is_dir = child.kind == Kind::Dir;
                if is_dir && !dropped_set.contains(&child.id) && !self.dirs.contains_key(&child.id)
                {
                    new_dirs.push(child.id);
                }
                after.push(Listed::new(parent, &child.name, is_dir, child.id));
                self.insert(child.id, parent, &child.name, child.kind);
            }
        }
        if dropped.iter().any(|d| !self.dirs.contains_key(d)) {
            self.prune_orphans();
        }
        before.sort_unstable();
        after.sort_unstable();
        Replaced {
            changed: before != after,
            new_dirs,
        }
    }

    /// Fraction of the arena taken by removed or replaced entries.
    pub fn garbage_ratio(&self) -> f64 {
        if self.slots.is_empty() {
            return 0.0;
        }
        #[allow(clippy::cast_precision_loss)] // a ratio; precision is irrelevant
        let ratio = (self.slots.len() - self.live) as f64 / self.slots.len() as f64;
        ratio
    }

    /// Rebuilds the arenas without dead entries. Run it when [`Self::garbage_ratio`] grows.
    pub fn compact(&mut self) {
        let mut fresh = Self::new(self.root_id, self.root_path.clone());
        fresh.slots.reserve(self.live);
        for slot in self.slots.iter().filter(|s| s.flags & DEAD == 0) {
            let name = push_bytes(&mut fresh.names, self.name_bytes(slot));
            let folded = push_bytes(&mut fresh.folded, self.folded_name(slot));
            if slot.flags & DIR != 0 {
                fresh.dirs.insert(slot.id, len_u32(fresh.slots.len()));
            }
            fresh.slots.push(Slot {
                name,
                folded,
                ..*slot
            });
            fresh.live += 1;
        }
        *self = fresh;
    }

    /// Marks slot `index` dead, and forgets it as a directory.
    fn kill_slot(&mut self, index: usize) {
        let slot = &mut self.slots[index];
        if slot.flags & DEAD != 0 {
            return;
        }
        slot.flags |= DEAD;
        let (id, is_dir) = (slot.id, slot.flags & DIR != 0);
        self.live -= 1;
        if is_dir && self.dirs.get(&id) == Some(&len_u32(index)) {
            self.dirs.remove(&id);
        }
    }

    /// Drops every entry whose parent chain no longer reaches the root. One pass; each
    /// directory's verdict is remembered.
    fn prune_orphans(&mut self) {
        const UNKNOWN: u8 = 0;
        const ATTACHED: u8 = 1;
        const ORPHAN: u8 = 2;
        let mut state = vec![UNKNOWN; self.slots.len()];
        let mut chain = Vec::new();
        for i in 0..self.slots.len() {
            if self.slots[i].flags & DEAD != 0 {
                continue;
            }
            chain.clear();
            let mut parent = self.slots[i].parent;
            let attached = loop {
                if parent == self.root_id {
                    break true;
                }
                let Some(&p) = self.dirs.get(&parent) else {
                    break false;
                };
                match state[p as usize] {
                    ATTACHED => break true,
                    ORPHAN => break false,
                    _ if chain.len() > MAX_DEPTH => break false, // a cycle; never loop forever
                    _ => {}
                }
                chain.push(p as usize);
                parent = self.slots[p as usize].parent;
            };
            let verdict = if attached { ATTACHED } else { ORPHAN };
            for &p in &chain {
                state[p] = verdict;
            }
            state[i] = verdict;
            if !attached {
                self.kill_slot(i);
            }
        }
    }

    fn name_bytes(&self, slot: &Slot) -> &[u8] {
        &self.names[slot.name as usize..(slot.name + slot.name_len) as usize]
    }

    fn name(&self, slot: &Slot) -> &str {
        // Names are pushed from `&str` or validated on load, so this is always UTF-8.
        std::str::from_utf8(self.name_bytes(slot)).unwrap_or_default()
    }

    fn folded_name(&self, slot: &Slot) -> &[u8] {
        &self.folded[slot.folded as usize..(slot.folded + slot.folded_len) as usize]
    }

    /// Builds the path of slot `index`. With `under`, returns `None` unless that directory
    /// is a proper ancestor.
    fn path_of(&self, index: u32, under: Option<FileId>) -> Option<PathBuf> {
        let mut parts = Vec::new();
        let mut slot = &self.slots[index as usize];
        let mut inside = under.is_none_or(|u| u == self.root_id);
        loop {
            parts.push(self.name(slot));
            if Some(slot.parent) == under {
                inside = true;
            }
            if slot.parent == self.root_id {
                break;
            }
            // A missing parent means the entry is detached (e.g. NTFS metadata files).
            slot = &self.slots[*self.dirs.get(&slot.parent)? as usize];
            if parts.len() > MAX_DEPTH {
                return None; // corrupt parent chain; never loop forever
            }
        }
        if !inside {
            return None;
        }
        let mut path = self.root_path.clone();
        path.extend(parts.iter().rev());
        Some(path)
    }
}

/// Deeper than any real tree (Windows paths top out near 32k UTF-16 units).
const MAX_DEPTH: usize = 16 * 1024;

/// Appends `\0bytes` to an arena and returns the offset of `bytes`.
fn push_bytes(arena: &mut Vec<u8>, bytes: &[u8]) -> u32 {
    arena.push(0);
    let start = len_u32(arena.len());
    arena.extend_from_slice(bytes);
    start
}

fn len_u32(n: usize) -> u32 {
    u32::try_from(n).expect("name index exceeds 4 GiB of names")
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) fn sample() -> NameIndex {
        let mut idx = NameIndex::new(5, r"C:\");
        idx.insert(10, 5, "Dev", Kind::Dir);
        idx.insert(11, 10, "PayGYM", Kind::Dir);
        idx.insert(12, 11, "PaymentService.cs", Kind::File);
        idx.insert(13, 11, "README.md", Kind::File);
        idx.insert(20, 5, "Windows", Kind::Dir);
        idx.insert(21, 20, "payment.log", Kind::File);
        idx
    }

    fn all(idx: &NameIndex) -> Vec<String> {
        idx.search(&NameQuery::new("", mtseek_core::CaseMode::Smart), None, 1)
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect()
    }

    fn file(id: FileId, name: &str) -> Child {
        Child {
            id,
            name: name.into(),
            kind: Kind::File,
        }
    }

    fn dir(id: FileId, name: &str) -> Child {
        Child {
            id,
            name: name.into(),
            kind: Kind::Dir,
        }
    }

    #[test]
    fn builds_paths_from_parent_links() {
        let idx = sample();
        assert_eq!(idx.dir_path(11).unwrap(), Path::new(r"C:\Dev\PayGYM"));
        assert_eq!(idx.dir_path(5).unwrap(), Path::new(r"C:\"));
        assert_eq!(idx.len(), 6);
        assert!(all(&idx).contains(&r"C:\Dev\PayGYM\PaymentService.cs".to_owned()));
    }

    #[test]
    fn reinserting_a_directory_moves_its_subtree() {
        let mut idx = sample();
        idx.insert(11, 20, "PayGYM-old", Kind::Dir);
        assert!(all(&idx).contains(&r"C:\Windows\PayGYM-old\PaymentService.cs".to_owned()));
        assert_eq!(idx.len(), 6);
    }

    #[test]
    fn hard_links_appear_in_every_directory() {
        let mut idx = sample();
        idx.insert(12, 20, "PaymentService.cs", Kind::File); // same file id, second link
        let found = idx.search(
            &NameQuery::new("PaymentService", mtseek_core::CaseMode::Smart),
            None,
            1,
        );
        assert_eq!(found.len(), 2);
    }

    #[test]
    fn replace_children_handles_create_rename_and_delete() {
        let mut idx = sample();
        // PayGYM: README.md deleted, PaymentService.cs renamed, a new file and a new dir.
        let done = idx.replace_children(vec![(
            11,
            vec![
                file(12, "Payments.cs"),
                file(30, "notes.txt"),
                dir(31, "docs"),
            ],
        )]);
        assert_eq!(done.new_dirs, [31]);
        assert!(done.changed);
        assert_eq!(
            all(&idx),
            [
                r"C:\Dev",
                r"C:\Dev\PayGYM",
                r"C:\Dev\PayGYM\Payments.cs",
                r"C:\Dev\PayGYM\docs",
                r"C:\Dev\PayGYM\notes.txt",
                r"C:\Windows",
                r"C:\Windows\payment.log",
            ]
        );
    }

    #[test]
    fn a_directory_moved_between_listings_keeps_its_subtree() {
        let mut idx = sample();
        let done = idx.replace_children(vec![
            (10, vec![]),                                           // PayGYM left Dev...
            (20, vec![file(21, "payment.log"), dir(11, "PayGYM")]), // ...for Windows
        ]);
        assert!(done.new_dirs.is_empty(), "a moved directory is not new");
        assert!(all(&idx).contains(&r"C:\Windows\PayGYM\README.md".to_owned()));
        assert_eq!(idx.len(), 6);
    }

    #[test]
    fn a_deleted_directory_takes_its_subtree_with_it() {
        let mut idx = sample();
        idx.replace_children(vec![(5, vec![dir(20, "Windows")])]); // Dev is gone
        assert_eq!(all(&idx), [r"C:\Windows", r"C:\Windows\payment.log"]);
        assert_eq!(idx.len(), 2);
        assert!(!idx.has_dir(11));
    }

    #[test]
    fn an_identical_listing_is_not_a_change() {
        let mut idx = sample();
        let same = vec![file(99, "PaymentService.cs"), file(13, "README.md")]; // new file id
        assert!(!idx.replace_children(vec![(11, same)]).changed);
        let renamed = vec![file(12, "PaymentService.cs"), file(13, "README.txt")];
        assert!(idx.replace_children(vec![(11, renamed)]).changed);
        assert_eq!(idx.len(), 6);
    }

    #[test]
    fn compact_drops_garbage_and_keeps_answers() {
        let mut idx = sample();
        idx.replace_children(vec![(11, vec![file(12, "Renamed.cs")])]);
        assert!(idx.garbage_ratio() > 0.0);
        let before = all(&idx);
        idx.compact();
        assert!(idx.garbage_ratio().abs() < f64::EPSILON);
        assert_eq!(all(&idx), before);
        assert_eq!(idx.len(), 5);
        assert_eq!(idx.dir_path(11).unwrap(), Path::new(r"C:\Dev\PayGYM"));
    }
}
