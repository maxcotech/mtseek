//! The NTFS facts the live index needs (ADR 0011): file reference numbers, FRN-carrying
//! directory listings and the change journal. Windows only; elsewhere every FRN is 0 and
//! there is no journal, so indexes can't be caught up and are rebuilt instead.

use std::fs::File;
use std::io;
use std::path::{Component, Path, Prefix};

use crate::{Checkpoint, Sd, no_sd};

/// Names whose presence or content changes which files ripgrep's walker lists below their
/// directory. `.git` makes `.gitignore` apply (ripgrep's `require_git`).
const CONTROL_NAMES: [&str; 4] = [".gitignore", ".ignore", ".rgignore", ".git"];

/// Whether `name` is an ignore-control file (Windows names are case-insensitive).
pub(crate) fn is_control(name: &str) -> bool {
    CONTROL_NAMES.iter().any(|c| c.eq_ignore_ascii_case(name))
}

/// One entry of an FRN-carrying directory listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Listed {
    pub(crate) name: String,
    pub(crate) frn: u64,
}

/// A directory's own FRN and its entries' names and FRNs.
#[derive(Debug, Clone, Default)]
pub(crate) struct Listing {
    pub(crate) frn: u64,
    pub(crate) entries: Vec<Listed>,
}

/// The drive letter of an absolute path (`C:\...` or `\\?\C:\...`).
pub(crate) fn drive_letter(path: &Path) -> Option<char> {
    match path.components().next()? {
        Component::Prefix(p) => match p.kind() {
            Prefix::Disk(d) | Prefix::VerbatimDisk(d) => Some(char::from(d).to_ascii_uppercase()),
            _ => None,
        },
        _ => None,
    }
}

/// Journal reasons that change who may read a file, but not its contents.
pub(crate) const SECURITY_CHANGE: u32 = 0x0000_0800;
/// Journal reasons that say nothing about contents or names: a permission change, and the
/// close that summarises it.
pub(crate) const NOT_CONTENT: u32 = SECURITY_CHANGE | 0x8000_0000;

/// Why the journal couldn't be read from a checkpoint.
#[derive(Debug)]
pub(crate) enum JournalError {
    /// No journal: not NTFS, not Windows, or the volume can't be opened.
    Unavailable,
    /// Recreated, or records past the checkpoint were discarded.
    Lost,
}

#[cfg(windows)]
mod imp {
    use super::{Checkpoint, File, JournalError, Listed, Listing, Path, Sd, io, no_sd};
    use mtseek_ntfs::{DirBuffer, Directory, Journal, VolumeError};

    pub(crate) fn file_frn(file: &File) -> u64 {
        mtseek_ntfs::file_ref_of(file).unwrap_or(0)
    }

    pub(crate) fn descriptor_of(file: &File) -> Sd {
        mtseek_ntfs::security::descriptor_of(file).map_or_else(|_| no_sd(), Sd::from)
    }

    pub(crate) fn descriptor_at(path: &Path) -> Sd {
        mtseek_ntfs::security::descriptor_at(path).map_or_else(|_| no_sd(), Sd::from)
    }

    pub(crate) fn list_dir(path: &Path) -> io::Result<Listing> {
        let dir = Directory::open(path)?;
        let frn = dir.file_ref()?;
        let mut raw = Vec::new();
        dir.read(&mut DirBuffer::default(), &mut raw)?;
        let entries = raw
            .into_iter()
            .map(|e| Listed {
                name: e.name,
                frn: e.file_ref,
            })
            .collect();
        Ok(Listing { frn, entries })
    }

    pub(crate) fn checkpoint(letter: char) -> Option<Checkpoint> {
        let info = Journal::open(letter).ok()?.info().ok()?;
        Some(Checkpoint {
            letter,
            journal_id: info.journal_id,
            usn: info.next_usn,
        })
    }

    pub(crate) fn read_journal(
        from: &Checkpoint,
        on_record: &mut dyn FnMut(u64, u64, u32),
    ) -> Result<Checkpoint, JournalError> {
        let journal = Journal::open(from.letter).map_err(|_| JournalError::Unavailable)?;
        let info = journal.info().map_err(|_| JournalError::Unavailable)?;
        if info.journal_id != from.journal_id {
            return Err(JournalError::Lost);
        }
        let next = journal
            .read(&info, from.usn, &mut |r| {
                on_record(r.file_ref, r.parent_ref, r.reason);
            })
            .map_err(|e| match e {
                VolumeError::JournalWrapped(_) => JournalError::Lost,
                _ => JournalError::Unavailable,
            })?;
        Ok(Checkpoint { usn: next, ..*from })
    }
}

#[cfg(not(windows))]
mod imp {
    use super::{Checkpoint, File, JournalError, Listing, Path, Sd, io, no_sd};

    pub(crate) fn file_frn(_: &File) -> u64 {
        0
    }

    pub(crate) fn descriptor_of(_: &File) -> Sd {
        no_sd()
    }

    pub(crate) fn descriptor_at(_: &Path) -> Sd {
        no_sd()
    }

    pub(crate) fn list_dir(path: &Path) -> io::Result<Listing> {
        let _ = std::fs::read_dir(path)?;
        Ok(Listing::default())
    }

    pub(crate) fn checkpoint(_: char) -> Option<Checkpoint> {
        None
    }

    pub(crate) fn read_journal(
        _: &Checkpoint,
        _: &mut dyn FnMut(u64, u64, u32),
    ) -> Result<Checkpoint, JournalError> {
        Err(JournalError::Unavailable)
    }
}

pub(crate) use imp::{checkpoint, descriptor_at, descriptor_of, file_frn, list_dir, read_journal};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_names_ignore_case() {
        assert!(is_control(".gitignore") && is_control(".GitIgnore") && is_control(".git"));
        assert!(!is_control("gitignore") && !is_control(".gitattributes"));
    }

    #[test]
    fn drive_letters() {
        assert_eq!(drive_letter(Path::new(r"C:\Dev")), Some('C'));
        assert_eq!(drive_letter(Path::new(r"\\?\d:\x")), Some('D'));
        assert_eq!(drive_letter(Path::new(r"\\server\share\x")), None);
        assert_eq!(drive_letter(Path::new("relative")), None);
    }

    #[cfg(windows)]
    #[test]
    fn listings_carry_the_same_frns_as_open_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "x").unwrap();
        std::fs::write(dir.path().join(".gitignore"), "y").unwrap();
        let listing = list_dir(dir.path()).unwrap();
        assert_eq!(listing.frn, mtseek_ntfs::file_ref(dir.path()).unwrap());
        let a = listing.entries.iter().find(|e| e.name == "a.txt").unwrap();
        let file = File::open(dir.path().join("a.txt")).unwrap();
        assert_eq!(a.frn, file_frn(&file));
        assert!(listing.entries.iter().any(|e| is_control(&e.name)));
    }
}
