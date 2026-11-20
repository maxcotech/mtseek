//! Building from the MFT, and bringing a saved index up to date from the USN journal.
//!
//! Catch-up needs no administrator rights. Unprivileged journal records carry no names,
//! but they do say which directory each change happened in. So catch-up collects the
//! directories that gained, lost or renamed an entry, lists each of them again, and walks
//! any directory that is new. A typical run touches a handful of directories instead of
//! a million files.

use std::collections::HashSet;
use std::io;
use std::path::Path;

use mtseek_ntfs::record::reason;
use mtseek_ntfs::{Journal, Volume, VolumeError, file_ref};

use crate::snapshot::Checkpoint;
use crate::walk::{Lister, walk_into};
use crate::{FileId, Kind, NameIndex};

/// Journal reasons that can add, remove or rename a directory entry. Data writes and
/// attribute changes don't affect names, and are most of the journal.
const NAME_CHANGES: u32 = reason::FILE_CREATE
    | reason::FILE_DELETE
    | reason::RENAME_OLD_NAME
    | reason::RENAME_NEW_NAME
    | reason::HARD_LINK_CHANGE
    | reason::REPARSE_POINT_CHANGE;

/// What a catch-up did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CatchUp {
    /// Where the index is now up to date.
    pub checkpoint: Checkpoint,
    /// Journal records read.
    pub records: u64,
    /// Indexed directories that changed and were listed again.
    pub changed_dirs: usize,
    /// Entries added by walking directories that are new since the checkpoint.
    pub walked_entries: u64,
    /// Whether the index's contents changed. When they didn't, saving it again is
    /// pointless (the journal records are cheap to skip again next time).
    pub changed: bool,
}

/// The journal position to record *before* a full build, so that changes made during the
/// build are replayed by the next catch-up. `None` if the volume has no journal.
pub fn checkpoint_now(letter: char) -> Option<Checkpoint> {
    let info = Journal::open(letter).ok()?.info().ok()?;
    Some(Checkpoint {
        journal_id: info.journal_id,
        usn: info.next_usn,
    })
}

/// Builds an index of the whole volume from its master file table. Needs administrator
/// rights; without them this fails with [`VolumeError::NotElevated`].
pub fn build_from_mft(letter: char) -> Result<(NameIndex, Checkpoint), VolumeError> {
    let volume = Volume::open(letter)?;
    let letter = volume.letter();
    let journal = Journal::open(letter)?.info()?;
    let root = format!(r"{letter}:\");
    let root_id =
        file_ref(Path::new(&root)).map_err(|source| VolumeError::Io { letter, source })?;
    let mut index = NameIndex::new(root_id, root);
    volume.enumerate(&mut |r| {
        let kind = if r.is_dir() { Kind::Dir } else { Kind::File };
        index.insert(r.file_ref, r.parent_ref, &r.name, kind);
    })?;
    let checkpoint = Checkpoint {
        journal_id: journal.journal_id,
        usn: journal.next_usn,
    };
    Ok((index, checkpoint))
}

/// Applies every change since `from` to `index`, whose files live on drive `letter`.
///
/// Fails with [`VolumeError::JournalWrapped`] when the journal no longer covers `from`
/// (it was recreated, or so much changed that old records were discarded). The index is
/// then unusable and must be rebuilt.
pub fn catch_up(
    index: &mut NameIndex,
    letter: char,
    from: Checkpoint,
) -> Result<CatchUp, VolumeError> {
    let journal = Journal::open(letter)?;
    let info = journal.info()?;
    if info.journal_id != from.journal_id {
        return Err(VolumeError::JournalWrapped(letter.to_ascii_uppercase()));
    }
    let mut touched = HashSet::new();
    let mut records = 0u64;
    let next = journal.read(&info, from.usn, &mut |r| {
        records += 1;
        if r.reason & NAME_CHANGES != 0 {
            touched.insert(r.parent_ref);
        }
    })?;
    let mut pending: Vec<FileId> = touched.into_iter().filter(|&d| index.has_dir(d)).collect();
    let changed_dirs = pending.len();

    // Paths come from the index, which is stale until the listings are applied. A renamed
    // directory has the wrong path until its parent's listing is in, and a different
    // directory may now sit at its old path. So a listing counts only if the directory at
    // the path has the expected id; the rest retry after each round that made progress.
    let mut lister = Lister::default();
    let mut walked_entries = 0;
    let mut changed = false;
    loop {
        let mut listings = Vec::new();
        let mut retry = Vec::new();
        for dir in pending {
            let Some(path) = index.dir_path(dir) else {
                continue; // dropped by an earlier listing: it was deleted
            };
            match lister.list(&path) {
                Ok((id, children)) if id == dir => listings.push((dir, children.to_vec())),
                Ok(_) => retry.push(dir),
                Err(e) if e.kind() == io::ErrorKind::NotFound => retry.push(dir),
                Err(_) => {} // unreadable now (permissions): keep what the index had
            }
        }
        if listings.is_empty() {
            break; // whatever is left was deleted, and its parent's listing dropped it
        }
        let replaced = index.replace_children(listings);
        changed |= replaced.changed;
        let starts = replaced
            .new_dirs
            .into_iter()
            .filter_map(|id| Some((index.dir_path(id)?, id)))
            .collect();
        walked_entries += walk_into(index, starts).entries;
        pending = retry;
    }
    Ok(CatchUp {
        checkpoint: Checkpoint {
            journal_id: info.journal_id,
            usn: next,
        },
        records,
        changed_dirs,
        walked_entries,
        changed,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{NameQuery, walk};
    use mtseek_core::CaseMode;
    use std::fs;
    use std::path::PathBuf;

    fn everything(index: &NameIndex) -> Vec<PathBuf> {
        index.search(&NameQuery::new("", CaseMode::Smart), None, 0)
    }

    fn letter(path: &Path) -> char {
        path.to_string_lossy().chars().next().unwrap()
    }

    /// Builds an index, changes the tree in every way catch-up must handle, catches up,
    /// and compares with a fresh walk. Runs as any user.
    #[test]
    fn catch_up_matches_a_fresh_walk() {
        let outside = tempfile::tempdir().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        for d in ["a/b/c", "moveme/deep", "gone/sub", "ren", "swap1", "swap2"] {
            fs::create_dir_all(root.join(d)).unwrap();
        }
        for f in [
            "a/b/c/file.txt",
            "moveme/deep/keep.rs",
            "gone/sub/x",
            "ren/inside.md",
            "swap1/one",
            "swap2/two",
            "plain.txt",
        ] {
            fs::write(root.join(f), "").unwrap();
        }
        fs::create_dir_all(outside.path().join("incoming/nested")).unwrap();
        fs::write(outside.path().join("incoming/nested/arrived.txt"), "").unwrap();

        let l = letter(root);
        let start = checkpoint_now(l).expect("NTFS volume with a journal");
        let (mut index, _) = walk(root).unwrap();

        // Create, delete, rename, move within, move in, move out, and a name swap.
        fs::create_dir_all(root.join("new/tree/deeper")).unwrap();
        fs::write(root.join("new/tree/deeper/fresh.txt"), "").unwrap();
        fs::write(root.join("a/b/c/added.txt"), "").unwrap();
        fs::remove_file(root.join("plain.txt")).unwrap();
        fs::remove_dir_all(root.join("gone")).unwrap();
        fs::rename(root.join("ren"), root.join("renamed")).unwrap();
        fs::write(root.join("renamed/after-rename.txt"), "").unwrap();
        fs::rename(root.join("moveme"), root.join("a/b/moved")).unwrap();
        fs::rename(outside.path().join("incoming"), root.join("a/incoming")).unwrap();
        fs::rename(root.join("a/b/c"), outside.path().join("left")).unwrap();
        fs::rename(root.join("swap1"), root.join("tmp")).unwrap();
        fs::rename(root.join("swap2"), root.join("swap1")).unwrap();
        fs::rename(root.join("tmp"), root.join("swap2")).unwrap();

        let done = catch_up(&mut index, l, start).unwrap();
        assert!(done.checkpoint.usn > start.usn);
        assert!(done.changed_dirs > 0 && done.records > 0);
        let (fresh, _) = walk(root).unwrap();
        assert_eq!(everything(&index), everything(&fresh));
        assert!(everything(&index).contains(&root.join("swap1").join("two")));

        // Nothing changed since: a second catch-up is a no-op.
        let again = catch_up(&mut index, l, done.checkpoint).unwrap();
        assert_eq!(everything(&index), everything(&fresh));
        assert_eq!(again.walked_entries, 0);
    }

    #[test]
    fn a_recreated_journal_is_reported_as_wrapped() {
        let dir = tempfile::tempdir().unwrap();
        let l = letter(dir.path());
        let (mut index, _) = walk(dir.path()).unwrap();
        let mut stale = checkpoint_now(l).unwrap();
        stale.journal_id ^= 1;
        assert!(matches!(
            catch_up(&mut index, l, stale),
            Err(VolumeError::JournalWrapped(_))
        ));
    }
}
