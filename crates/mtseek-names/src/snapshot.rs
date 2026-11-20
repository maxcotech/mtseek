//! Saving an index to disk and loading it back, with the journal position it is valid at.
//!
//! A snapshot is a cache: deleting it never changes an answer, only how long the next
//! answer takes. Loading never trusts the file. Every offset is checked, and a file that
//! is truncated, corrupt or from another format version is rejected with
//! [`io::ErrorKind::InvalidData`], after which the caller rebuilds.
//!
//! # Format (little-endian)
//!
//! ```text
//! "MTSKNIDX" u32:version u32:origin u64:journal_id i64:usn u64:root_id
//! u32:root_path_len root_path(UTF-8)
//! u64:slot_count u64:names_len u64:folded_len
//! slot_count x { u64:id u64:parent u32:name u32:name_len u32:folded u32:folded_len u8:flags }
//! names folded "MTSKEND!"
//! ```
//!
//! Only live entries are written, so a saved index is always compact.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::{DEAD, DIR, FileId, NameIndex, Slot, push_bytes};

const MAGIC: &[u8; 8] = b"MTSKNIDX";
const END: &[u8; 8] = b"MTSKEND!";
const VERSION: u32 = 1;
const SLOT_BYTES: usize = 8 + 8 + 4 * 4 + 1;

/// Where in the volume's change journal an index is up to date.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Checkpoint {
    /// The journal's identity. A different id means the journal was recreated.
    pub journal_id: u64,
    /// The first journal position not yet applied.
    pub usn: i64,
}

/// How an index was first built.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// From the NTFS master file table: the whole volume, rooted at the drive.
    Mft,
    /// From a directory walk of one root.
    Walk,
}

/// An index, how it was built, and the journal position it reflects.
#[derive(Debug, Clone)]
pub struct Snapshot {
    /// The index.
    pub index: NameIndex,
    /// How it was first built.
    pub origin: Origin,
    /// The journal position it reflects.
    pub checkpoint: Checkpoint,
}

impl Snapshot {
    /// Writes the snapshot to `path` atomically: a reader sees the old file or the new one.
    pub fn save(&self, path: &Path) -> io::Result<()> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        let mut temp = path.as_os_str().to_owned();
        temp.push(format!(".{}.tmp", std::process::id()));
        let temp = PathBuf::from(temp);
        fs::write(&temp, self.to_bytes()?)?;
        fs::rename(&temp, path).inspect_err(|_| {
            let _ = fs::remove_file(&temp);
        })
    }

    /// Reads a snapshot written by [`Self::save`].
    pub fn load(path: &Path) -> io::Result<Self> {
        Self::from_bytes(&fs::read(path)?)
    }

    /// The serialised form. Fails only if the root path is not valid Unicode.
    pub fn to_bytes(&self) -> io::Result<Vec<u8>> {
        let index = &self.index;
        let root = index.root_path.to_str().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "root path is not Unicode")
        })?;
        let mut names = Vec::with_capacity(index.names.len());
        let mut folded = Vec::with_capacity(index.folded.len());
        let mut slots = Vec::with_capacity(index.live * SLOT_BYTES);
        let mut count = 0u64;
        for slot in index.slots.iter().filter(|s| s.flags & DEAD == 0) {
            let name = push_bytes(&mut names, index.name_bytes(slot));
            let folded_at = push_bytes(&mut folded, index.folded_name(slot));
            slots.extend_from_slice(&slot.id.to_le_bytes());
            slots.extend_from_slice(&slot.parent.to_le_bytes());
            slots.extend_from_slice(&name.to_le_bytes());
            slots.extend_from_slice(&slot.name_len.to_le_bytes());
            slots.extend_from_slice(&folded_at.to_le_bytes());
            slots.extend_from_slice(&slot.folded_len.to_le_bytes());
            slots.push(slot.flags & DIR);
            count += 1;
        }

        let mut out = Vec::with_capacity(96 + root.len() + slots.len() + names.len() * 2);
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&VERSION.to_le_bytes());
        out.extend_from_slice(&u32::from(self.origin == Origin::Mft).to_le_bytes());
        out.extend_from_slice(&self.checkpoint.journal_id.to_le_bytes());
        out.extend_from_slice(&self.checkpoint.usn.to_le_bytes());
        out.extend_from_slice(&index.root_id.to_le_bytes());
        out.extend_from_slice(&crate::len_u32(root.len()).to_le_bytes());
        out.extend_from_slice(root.as_bytes());
        out.extend_from_slice(&count.to_le_bytes());
        out.extend_from_slice(&(names.len() as u64).to_le_bytes());
        out.extend_from_slice(&(folded.len() as u64).to_le_bytes());
        out.extend_from_slice(&slots);
        out.extend_from_slice(&names);
        out.extend_from_slice(&folded);
        out.extend_from_slice(END);
        Ok(out)
    }

    /// Parses [`Self::to_bytes`] output. Never panics; bad input is `InvalidData`.
    pub fn from_bytes(bytes: &[u8]) -> io::Result<Self> {
        let mut r = Reader { bytes, at: 0 };
        if r.take(8)? != MAGIC || r.u32()? != VERSION {
            return Err(invalid("not an mtseek name index, or another version"));
        }
        let origin = match r.u32()? {
            0 => Origin::Walk,
            1 => Origin::Mft,
            _ => return Err(invalid("unknown origin")),
        };
        let checkpoint = Checkpoint {
            journal_id: r.u64()?,
            usn: i64::from_le_bytes(r.array()?),
        };
        let root_id: FileId = r.u64()?;
        let root_len = r.u32()? as usize;
        let root = std::str::from_utf8(r.take(root_len)?).map_err(|_| invalid("root path"))?;
        let count = usize::try_from(r.u64()?).map_err(|_| invalid("slot count"))?;
        let names_len = usize::try_from(r.u64()?).map_err(|_| invalid("names length"))?;
        let folded_len = usize::try_from(r.u64()?).map_err(|_| invalid("folded length"))?;
        let slot_bytes = r.take(
            count
                .checked_mul(SLOT_BYTES)
                .ok_or_else(|| invalid("slot count"))?,
        )?;
        let names = r.take(names_len)?;
        let folded = r.take(folded_len)?;
        if r.take(8)? != END || r.at != bytes.len() {
            return Err(invalid("trailing bytes or missing end marker"));
        }
        if u32::try_from(names_len.max(folded_len)).is_err() {
            return Err(invalid("arena larger than 4 GiB"));
        }

        let mut index = NameIndex::new(root_id, root);
        // Room for catch-ups to add a little without doubling the arenas.
        index.names = with_headroom(names);
        index.folded = with_headroom(folded);
        index.slots.reserve(count + count / 16);
        let (mut last_name, mut last_folded) = (0u32, 0u32);
        let (raw_slots, _) = slot_bytes.as_chunks::<SLOT_BYTES>(); // exact: length checked above
        for (i, raw) in raw_slots.iter().enumerate() {
            let mut s = Reader { bytes: raw, at: 0 };
            let slot = Slot {
                id: s.u64()?,
                parent: s.u64()?,
                name: s.u32()?,
                name_len: s.u32()?,
                folded: s.u32()?,
                folded_len: s.u32()?,
                flags: s.take(1)?[0],
            };
            // Search maps hits to slots by binary search, so offsets must ascend, and every
            // name must be a valid range of its arena, preceded by a NUL separator.
            let in_arena = |at: u32, len: u32, arena: &[u8]| {
                at >= 1
                    && arena.get(at as usize - 1) == Some(&0)
                    && (at as usize)
                        .checked_add(len as usize)
                        .is_some_and(|end| end <= arena.len())
            };
            if slot.flags & !DIR != 0
                || slot.name < last_name
                || slot.folded < last_folded
                || !in_arena(slot.name, slot.name_len, &index.names)
                || !in_arena(slot.folded, slot.folded_len, &index.folded)
                || std::str::from_utf8(index.name_bytes(&slot)).is_err()
            {
                return Err(invalid("corrupt entry"));
            }
            (last_name, last_folded) = (slot.name, slot.folded);
            if slot.flags & DIR != 0 {
                index.dirs.insert(slot.id, crate::len_u32(i));
            }
            index.slots.push(slot);
        }
        index.live = index.slots.len();
        Ok(Self {
            index,
            origin,
            checkpoint,
        })
    }
}

fn with_headroom(bytes: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(bytes.len() + bytes.len() / 16);
    v.extend_from_slice(bytes);
    v
}

fn invalid(what: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("bad name index snapshot: {what}"),
    )
}

struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> io::Result<&'a [u8]> {
        let end = self.at.checked_add(n).ok_or_else(|| invalid("length"))?;
        let slice = self
            .bytes
            .get(self.at..end)
            .ok_or_else(|| invalid("truncated"))?;
        self.at = end;
        Ok(slice)
    }

    fn array<const N: usize>(&mut self) -> io::Result<[u8; N]> {
        let mut a = [0; N];
        a.copy_from_slice(self.take(N)?);
        Ok(a)
    }

    fn u32(&mut self) -> io::Result<u32> {
        Ok(u32::from_le_bytes(self.array()?))
    }

    fn u64(&mut self) -> io::Result<u64> {
        Ok(u64::from_le_bytes(self.array()?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::sample;
    use crate::{Child, Kind, NameQuery};
    use mtseek_core::CaseMode;
    use proptest::prelude::*;

    fn snapshot(index: NameIndex) -> Snapshot {
        Snapshot {
            index,
            origin: Origin::Walk,
            checkpoint: Checkpoint {
                journal_id: 7,
                usn: 1234,
            },
        }
    }

    fn everything(index: &NameIndex) -> Vec<PathBuf> {
        index.search(&NameQuery::new("", CaseMode::Smart), None, 1)
    }

    #[test]
    fn roundtrips_and_drops_garbage() {
        let mut index = sample();
        index.replace_children(vec![(
            11,
            vec![Child {
                id: 12,
                name: "Renamed.cs".into(),
                kind: Kind::File,
            }],
        )]);
        let saved = snapshot(index);
        let loaded = Snapshot::from_bytes(&saved.to_bytes().unwrap()).unwrap();
        assert_eq!(loaded.checkpoint, saved.checkpoint);
        assert_eq!(loaded.origin, Origin::Walk);
        assert_eq!(everything(&loaded.index), everything(&saved.index));
        assert!(loaded.index.garbage_ratio().abs() < f64::EPSILON);
        assert_eq!(loaded.index.dir_path(11), saved.index.dir_path(11));
        let q = NameQuery::new("renamed", CaseMode::Smart);
        assert_eq!(loaded.index.search(&q, None, 2).len(), 1);
    }

    #[test]
    fn saves_atomically_to_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cache").join("x.idx");
        snapshot(sample()).save(&path).unwrap();
        let loaded = Snapshot::load(&path).unwrap();
        assert_eq!(everything(&loaded.index), everything(&sample()));
        assert_eq!(
            fs::read_dir(path.parent().unwrap()).unwrap().count(),
            1,
            "no temp left"
        );
    }

    #[test]
    fn rejects_truncation_and_other_versions() {
        let bytes = snapshot(sample()).to_bytes().unwrap();
        for cut in [0, 8, 40, bytes.len() / 2, bytes.len() - 1] {
            let err = Snapshot::from_bytes(&bytes[..cut]).unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::InvalidData, "cut at {cut}");
        }
        let mut other = bytes.clone();
        other[8] = 99;
        assert!(Snapshot::from_bytes(&other).is_err());
    }

    proptest! {
        #[test]
        fn corrupt_bytes_never_panic(at in any::<prop::sample::Index>(), value in any::<u8>()) {
            let mut bytes = snapshot(sample()).to_bytes().unwrap();
            let i = at.index(bytes.len());
            bytes[i] = value;
            if let Ok(loaded) = Snapshot::from_bytes(&bytes) {
                let _ = everything(&loaded.index); // whatever loads must be searchable
            }
        }
    }
}
