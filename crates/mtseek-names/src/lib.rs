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
//! # Sources
//!
//! The index doesn't care where entries come from: NTFS MFT enumeration plus USN journal
//! updates (`mtseek-ntfs`, needs admin), or [`walk`] as the portable fallback.

mod query;
mod walk;

pub use query::NameQuery;
pub use walk::{WalkStats, walk};

use std::collections::HashMap;
use std::path::PathBuf;

/// A file's identity. On NTFS, its file reference number; for walked trees, a counter.
pub type FileId = u64;

/// Whether an entry is a file or a directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// A file (or anything that isn't a directory).
    File,
    /// A directory.
    Dir,
}

/// A change to apply to the index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    /// Create, rename or move: the entry with this id now has this parent and name.
    Upsert {
        /// The entry's id.
        id: FileId,
        /// Its parent directory's id.
        parent: FileId,
        /// Its file name.
        name: String,
        /// File or directory.
        kind: Kind,
    },
    /// The entry with this id no longer exists.
    Remove {
        /// The entry's id.
        id: FileId,
    },
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
#[derive(Debug, Clone)]
pub struct NameIndex {
    root_id: FileId,
    root_path: PathBuf,
    slots: Vec<Slot>,
    by_id: HashMap<FileId, u32>,
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
            by_id: HashMap::new(),
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

    /// Bytes held by the name arenas and entry table. Useful for status displays.
    pub fn memory_bytes(&self) -> usize {
        self.names.capacity()
            + self.folded.capacity()
            + self.slots.capacity() * size_of::<Slot>()
            + self.by_id.capacity() * (size_of::<FileId>() + size_of::<u32>() + 1)
    }

    /// Applies a change.
    pub fn apply(&mut self, change: Change) {
        match change {
            Change::Upsert {
                id,
                parent,
                name,
                kind,
            } => self.upsert(id, parent, &name, kind),
            Change::Remove { id } => self.remove(id),
        }
    }

    /// Adds an entry, or replaces the one with the same id (rename or move).
    pub fn upsert(&mut self, id: FileId, parent: FileId, name: &str, kind: Kind) {
        if id == self.root_id {
            return; // the root is implicit
        }
        self.remove(id);
        let folded_name = name.to_lowercase();
        let slot = Slot {
            id,
            parent,
            name: push_name(&mut self.names, name),
            name_len: len_u32(name.len()),
            folded: push_name(&mut self.folded, &folded_name),
            folded_len: len_u32(folded_name.len()),
            flags: if kind == Kind::Dir { DIR } else { 0 },
        };
        self.by_id.insert(id, len_u32(self.slots.len()));
        self.slots.push(slot);
        self.live += 1;
    }

    /// Removes an entry if present.
    pub fn remove(&mut self, id: FileId) {
        if let Some(index) = self.by_id.remove(&id) {
            self.slots[index as usize].flags |= DEAD;
            self.live -= 1;
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
        for slot in self.slots.iter().filter(|s| s.flags & DEAD == 0) {
            let kind = if slot.flags & DIR == 0 {
                Kind::File
            } else {
                Kind::Dir
            };
            fresh.upsert(slot.id, slot.parent, self.name(slot), kind);
        }
        *self = fresh;
    }

    /// The full path of a live entry, or `None` if it is unknown or detached from the root.
    pub fn path(&self, id: FileId) -> Option<PathBuf> {
        let index = *self.by_id.get(&id)?;
        self.path_of(index, None)
    }

    /// The id of the entry at `path` relative to the root, following names. Slow (linear);
    /// meant for resolving a search scope once, not per result.
    pub fn id_of(&self, relative: &std::path::Path) -> Option<FileId> {
        let mut current = self.root_id;
        for component in relative.components() {
            let wanted = component.as_os_str().to_str()?;
            current = self
                .slots
                .iter()
                .find(|s| {
                    s.flags & DEAD == 0
                        && s.parent == current
                        && self.name(s).eq_ignore_ascii_case(wanted)
                })?
                .id;
        }
        Some(current)
    }

    fn name(&self, slot: &Slot) -> &str {
        let bytes = &self.names[slot.name as usize..(slot.name + slot.name_len) as usize];
        // Names are only ever pushed from `&str`, so the slice is valid UTF-8.
        std::str::from_utf8(bytes).unwrap_or_default()
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
            slot = &self.slots[*self.by_id.get(&slot.parent)? as usize];
            if parts.len() > 2048 {
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

/// Appends `\0name` and returns the offset of `name`.
fn push_name(arena: &mut Vec<u8>, name: &str) -> u32 {
    arena.push(0);
    let start = len_u32(arena.len());
    arena.extend_from_slice(name.as_bytes());
    start
}

fn len_u32(n: usize) -> u32 {
    u32::try_from(n).expect("name index exceeds 4 GiB of names")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    pub(crate) fn sample() -> NameIndex {
        let mut idx = NameIndex::new(5, r"C:\");
        idx.upsert(10, 5, "Dev", Kind::Dir);
        idx.upsert(11, 10, "PayGYM", Kind::Dir);
        idx.upsert(12, 11, "PaymentService.cs", Kind::File);
        idx.upsert(13, 11, "README.md", Kind::File);
        idx.upsert(20, 5, "Windows", Kind::Dir);
        idx.upsert(21, 20, "payment.log", Kind::File);
        idx
    }

    #[test]
    fn builds_paths_from_parent_links() {
        let idx = sample();
        assert_eq!(
            idx.path(12).unwrap(),
            Path::new(r"C:\Dev\PayGYM\PaymentService.cs")
        );
        assert_eq!(idx.len(), 6);
    }

    #[test]
    fn rename_and_move_update_one_entry() {
        let mut idx = sample();
        idx.upsert(11, 20, "PayGYM-old", Kind::Dir); // move + rename the directory
        assert_eq!(
            idx.path(12).unwrap(),
            Path::new(r"C:\Windows\PayGYM-old\PaymentService.cs")
        );
        assert_eq!(idx.len(), 6);
    }

    #[test]
    fn removed_entries_detach_their_children() {
        let mut idx = sample();
        idx.remove(11);
        assert_eq!(idx.path(11), None);
        assert_eq!(idx.path(12), None, "orphans have no path");
        assert_eq!(idx.len(), 5);
    }

    #[test]
    fn compact_drops_garbage_and_keeps_answers() {
        let mut idx = sample();
        idx.upsert(12, 11, "Renamed.cs", Kind::File);
        idx.remove(13);
        assert!(idx.garbage_ratio() > 0.0);
        let before = idx.path(12);
        idx.compact();
        assert!(idx.garbage_ratio().abs() < f64::EPSILON);
        assert_eq!(idx.path(12), before);
        assert_eq!(idx.len(), 5);
    }

    #[test]
    fn resolves_relative_paths_to_ids() {
        let idx = sample();
        assert_eq!(idx.id_of(Path::new(r"dev\paygym")), Some(11));
        assert_eq!(idx.id_of(Path::new("")), Some(5));
        assert_eq!(idx.id_of(Path::new("nope")), None);
    }
}
