//! Invariant 3 for the content index (ADR 0011): after any sequence of changes, an index
//! caught up from the change journal holds exactly what a fresh build holds. Same files,
//! same stored bytes, same directories, control files and errors, with the same membership
//! bits for every coverage filter (ADR 0012), and the same security descriptors, including
//! after permission changes and their inheritance (ADR 0014).
//!
//! Windows only: the journal is NTFS's. Needs no admin rights.

#![cfg(windows)]

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use mtseek_core::{Filter, Scope};
use mtseek_index::{CatchUp, Index, Location, Persist, membership};
use mtseek_walk::Options;
use proptest::prelude::*;

/// A file as the index holds it: its stored bytes (`None` if read from disk), flags and
/// security descriptor.
type Held = (Option<Vec<u8>>, u8, Vec<u8>);

/// A directory or control file: root, path, flags, security descriptor.
type Tracked = (u32, PathBuf, u8, Vec<u8>);

/// Everything an index answers with, keyed by (root, relative path).
#[derive(Debug, PartialEq, Eq)]
struct View {
    files: BTreeMap<(usize, PathBuf), Held>,
    dirs: Vec<Tracked>,
    controls: Vec<Tracked>,
    errors: Vec<String>,
}

fn view(index: &Index) -> View {
    let mut files = BTreeMap::new();
    for (segment, skip) in index.layers() {
        let mut reader = segment.block_reader().expect("block reader");
        for id in 0..segment.len() {
            if skip.binary_search(&id).is_ok() {
                continue;
            }
            let bytes = match segment.location(id) {
                Location::Empty => Some(Vec::new()),
                Location::External => None,
                Location::Block { block, offset, len } => {
                    let raw = reader.read(block).expect("block reads");
                    Some(raw[offset as usize..(offset + len) as usize].to_vec())
                }
            };
            let held = (bytes, segment.flags(id), segment.descriptor(id).to_vec());
            let previous = files.insert(segment.path(id), held);
            assert!(
                previous.is_none(),
                "{:?} is in two layers",
                segment.path(id)
            );
        }
    }
    let newest = index.delta().unwrap_or_else(|| index.base());
    let paths = |entries: Vec<mtseek_index::Entry>| {
        let mut out: Vec<Tracked> = entries
            .into_iter()
            .map(|e| (e.root, e.path, e.flags, e.sd.to_vec()))
            .collect();
        out.sort();
        out
    };
    let mut errors: Vec<String> = index
        .errors()
        .iter()
        .map(|e| {
            format!(
                "{:?} {} [{}]: {}",
                e.root,
                e.path.display(),
                e.flags,
                e.message
            )
        })
        .collect();
    errors.sort();
    View {
        files,
        dirs: paths(newest.dirs().expect("dirs")),
        controls: paths(newest.controls().expect("controls")),
        errors,
    }
}

/// A change to the tree. Indexes pick from small fixed sets of names, so changes collide.
#[derive(Debug, Clone)]
enum Op {
    Write {
        dir: usize,
        file: usize,
        text: usize,
    },
    Append {
        dir: usize,
        file: usize,
        text: usize,
    },
    Delete {
        dir: usize,
        file: usize,
    },
    RenameFile {
        from: (usize, usize),
        to: (usize, usize),
    },
    MakeDir {
        dir: usize,
    },
    RemoveDir {
        dir: usize,
    },
    RenameDir {
        from: usize,
        to: usize,
    },
    Ignore {
        dir: usize,
        pattern: usize,
    },
    RemoveIgnore {
        dir: usize,
    },
    MakeGit {
        dir: usize,
    },
    Hide {
        dir: usize,
        file: usize,
        hidden: bool,
    },
    HideDir {
        dir: usize,
        hidden: bool,
    },
    /// Writes `.git\info\exclude` in a repository.
    Exclude {
        dir: usize,
        pattern: usize,
    },
    /// Writes or removes an ignore file in the folder above the root, or makes it a
    /// repository.
    Above {
        what: usize,
    },
    /// Gives a file or folder (`file: None`) one of two DACLs that both let this account
    /// read everything, so a fresh build reads the same. A folder's change is inherited by
    /// what is inside it.
    Permit {
        dir: usize,
        file: Option<usize>,
        dacl: usize,
    },
}

/// DACLs for [`Op::Permit`]; `{}` is this account's SID.
const DACLS: &[&str] = &["D:P(A;OICI;FA;;;{})", "D:P(A;OICI;FA;;;{})(A;OICI;FR;;;WD)"];

impl Op {
    /// Whether the change is above the indexed folder, where catch-up rebuilds.
    fn is_above(&self) -> bool {
        matches!(self, Op::Above { .. })
    }
}

const DIRS: &[&str] = &["", "a", "a/b", "c", "c/d", ".h"];
const FILES: &[&str] = &["x.txt", "y.rs", "z.md", ".dot", "w.bin"];
const TEXTS: &[&[u8]] = &[
    b"needle one\n",
    b"other text\n",
    b"",
    b"bin\0ary\n",
    b"\xFF\xFEu\0t\0f\0",
];
const PATTERNS: &[&str] = &["x.txt\n", "*.rs\n", "b/\n", "d\n", "!y.rs\n*\n"];
/// Changes above the root: an ignore file there with each pattern, removing it, `.git`.
const ABOVE: &[&str] = &["*.md\n", "a/\n", "!z.md\n", "", "git"];

fn op() -> impl Strategy<Value = Op> {
    let d = 0..DIRS.len();
    let f = 0..FILES.len();
    let t = 0..TEXTS.len();
    prop_oneof![
        4 => (d.clone(), f.clone(), t.clone()).prop_map(|(dir, file, text)| Op::Write { dir, file, text }),
        2 => (d.clone(), f.clone(), t.clone()).prop_map(|(dir, file, text)| Op::Append { dir, file, text }),
        2 => (d.clone(), f.clone()).prop_map(|(dir, file)| Op::Delete { dir, file }),
        2 => ((d.clone(), f.clone()), (d.clone(), f.clone())).prop_map(|(from, to)| Op::RenameFile { from, to }),
        2 => d.clone().prop_map(|dir| Op::MakeDir { dir }),
        1 => d.clone().prop_map(|dir| Op::RemoveDir { dir }),
        1 => (d.clone(), d.clone()).prop_map(|(from, to)| Op::RenameDir { from, to }),
        1 => (d.clone(), 0..PATTERNS.len()).prop_map(|(dir, pattern)| Op::Ignore { dir, pattern }),
        1 => d.clone().prop_map(|dir| Op::RemoveIgnore { dir }),
        1 => d.clone().prop_map(|dir| Op::MakeGit { dir }),
        1 => (d.clone(), f, any::<bool>()).prop_map(|(dir, file, hidden)| Op::Hide { dir, file, hidden }),
        1 => (d.clone(), any::<bool>()).prop_map(|(dir, hidden)| Op::HideDir { dir, hidden }),
        1 => (d, 0..PATTERNS.len()).prop_map(|(dir, pattern)| Op::Exclude { dir, pattern }),
        1 => (0..ABOVE.len()).prop_map(|what| Op::Above { what }),
        2 => (0..DIRS.len(), proptest::option::of(0..FILES.len()), 0..DACLS.len())
            .prop_map(|(dir, file, dacl)| Op::Permit { dir, file, dacl }),
    ]
}

/// Applies `op` under `root`. Changes that don't make sense right now (a missing source,
/// removing the root) are skipped; failures are fine, the tree is whatever it is after.
fn apply(root: &Path, op: &Op) {
    let dir = |d: usize| root.join(DIRS[d]);
    let file = |d: usize, f: usize| dir(d).join(FILES[f]);
    match *op {
        Op::Write {
            dir: d,
            file: f,
            text,
        } => {
            let _ = fs::create_dir_all(dir(d));
            let _ = fs::write(file(d, f), TEXTS[text]);
        }
        Op::Append {
            dir: d,
            file: f,
            text,
        } => {
            if let Ok(mut bytes) = fs::read(file(d, f)) {
                bytes.extend_from_slice(TEXTS[text]);
                let _ = fs::write(file(d, f), bytes);
            }
        }
        Op::Delete { dir: d, file: f } => {
            let _ = fs::remove_file(file(d, f));
        }
        Op::RenameFile { from, to } => {
            if dir(to.0).is_dir() {
                let _ = fs::rename(file(from.0, from.1), file(to.0, to.1));
            }
        }
        Op::MakeDir { dir: d } => {
            let _ = fs::create_dir_all(dir(d));
        }
        Op::RemoveDir { dir: d } => {
            if d != 0 {
                let _ = fs::remove_dir_all(dir(d));
            }
        }
        Op::RenameDir { from, to } => {
            let (from, to) = (dir(from), dir(to));
            if from != root
                && !to.exists()
                && !to.starts_with(&from)
                && to.parent().is_some_and(Path::is_dir)
            {
                let _ = fs::rename(from, to);
            }
        }
        Op::Ignore { dir: d, pattern } => {
            if dir(d).is_dir() {
                let _ = fs::write(dir(d).join(".gitignore"), PATTERNS[pattern]);
            }
        }
        Op::RemoveIgnore { dir: d } => {
            let _ = fs::remove_file(dir(d).join(".gitignore"));
        }
        Op::MakeGit { dir: d } => {
            // Like `git init`: `.git` with an `info` folder (a later `info` isn't noticed).
            if dir(d).is_dir() {
                let _ = fs::create_dir_all(dir(d).join(".git").join("info"));
            }
        }
        Op::Exclude { dir: d, pattern } => {
            let info = dir(d).join(".git").join("info");
            if info.is_dir() {
                let _ = fs::write(info.join("exclude"), PATTERNS[pattern]);
            }
        }
        Op::Above { what } => change_above(root, what),
        Op::Permit {
            dir: d,
            file: f,
            dacl,
        } => {
            let path = f.map_or_else(|| dir(d), |f| file(d, f));
            if path.exists() {
                let sid = mtseek_ntfs::pipe::user_sid().expect("own SID");
                let _ = mtseek_ntfs::security::set_dacl(&path, &DACLS[dacl].replace("{}", &sid));
            }
        }
        Op::Hide {
            dir: d,
            file: f,
            hidden,
        } => {
            if file(d, f).exists() {
                let _ = Command::new("attrib")
                    .arg(if hidden { "+h" } else { "-h" })
                    .arg(file(d, f))
                    .output();
            }
        }
        Op::HideDir { dir: d, hidden } => {
            if d != 0 && dir(d).is_dir() {
                let _ = Command::new("attrib")
                    .arg(if hidden { "+h" } else { "-h" })
                    .arg(dir(d))
                    .output();
            }
        }
    }
}

/// Applies change `ABOVE[what]` in the folder above `root`.
fn change_above(root: &Path, what: usize) {
    let above = root.parent().expect("the root has a parent");
    match ABOVE[what] {
        "" => {
            let _ = fs::remove_file(above.join(".gitignore"));
        }
        "git" => {
            let _ = fs::create_dir_all(above.join(".git").join("info"));
        }
        pattern => {
            let _ = fs::write(above.join(".gitignore"), pattern);
        }
    }
}

fn scope(root: &Path, level: u8) -> Scope {
    Scope::new([root]).with_filter(Filter::unrestricted(level))
}

/// A scope with one of the four coverage filters (`which` in 0..4).
fn coverage_scope(root: &Path, which: u8) -> Scope {
    Scope::new([root]).with_filter(membership::filter(1 << which))
}

/// Catches `index` up and checks it against a fresh build of the same scope. A rebuild is
/// only allowed after a change above the root (`above`); then the index is rebuilt.
fn check(index: &mut Index, scope: &Scope, cache: &Path, above: bool) -> Result<(), TestCaseError> {
    let done = index
        .catch_up(scope, Options::default())
        .expect("catch-up works");
    if let CatchUp::NeedsRebuild(reason) = done {
        prop_assert!(above && reason.contains("above"), "{done:?}");
        // A new path: Windows can't replace the old index's files while they are open.
        let path = cache.join(format!("rebuilt-{}", index.meta().build_id));
        *index = Index::build(scope, Options::default(), &path)
            .expect("rebuild")
            .0;
        return Ok(());
    }
    let (fresh, _) =
        Index::build(scope, Options::default(), &cache.join("fresh")).expect("fresh build");
    let (caught_up, expected) = (view(index), view(&fresh));
    prop_assert_eq!(caught_up, expected, "after {:?}", done);
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(
        std::env::var("PROPTEST_CASES").ok().and_then(|v| v.parse().ok()).unwrap_or(32)
    ))]

    #[test]
    /// `later` keeps deltas in memory as the daemon does (ADR 0013). After each batch,
    /// `after` saves them (1), or drops the index and opens it from disk again (2): a
    /// daemon that stopped without saving.
    fn a_caught_up_index_equals_a_fresh_build(
        setup in proptest::collection::vec(op(), 0..12),
        batches in proptest::collection::vec(
            (proptest::collection::vec(op(), 1..8), 0u8..3),
            1..4,
        ),
        which in 0u8..4,
        later in any::<bool>(),
    ) {
        let tree = tempfile::tempdir().expect("temp dir");
        let cache = tempfile::tempdir().expect("temp dir");
        // The root is one level down, so that changes above it stay inside the test.
        let root = tree.path().join("root");
        fs::create_dir(&root).expect("mkdir");
        for op in &setup {
            apply(&root, op);
        }
        let scope = coverage_scope(&root, which);
        let (mut index, _) =
            Index::build(&scope, Options::default(), &cache.path().join("idx")).expect("build");
        let persist = if later { Persist::Later } else { Persist::Now };
        index.set_persist(persist);
        for (batch, after) in &batches {
            for op in batch {
                apply(&root, op);
            }
            let above = batch.iter().any(Op::is_above);
            check(&mut index, &scope, cache.path(), above)?;
            index.set_persist(persist);
            prop_assert!(later || !index.unsaved());
            match after {
                1 => {
                    index.save().expect("save");
                    prop_assert!(!index.unsaved());
                }
                2 => {
                    let key = mtseek_index::scope_key(&scope).expect("key");
                    let path = index.path().to_path_buf();
                    drop(index);
                    index = Index::open(&path, &key).expect("the base is still there");
                    index.set_persist(persist);
                }
                _ => {}
            }
        }
        // A restart after the last batch catches up from what was saved.
        let above = batches.iter().any(|(batch, _)| batch.iter().any(Op::is_above));
        check(&mut index, &scope, cache.path(), above)?;
    }
}

#[test]
fn nothing_changed_reads_no_files() {
    let tree = tempfile::tempdir().unwrap();
    fs::write(tree.path().join("a.txt"), "hello").unwrap();
    let cache = tempfile::tempdir().unwrap();
    let scope = scope(tree.path(), 0);
    let (mut index, _) =
        Index::build(&scope, Options::default(), &cache.path().join("idx")).unwrap();
    // Other activity on the drive may produce records, but none under this tree.
    let done = index.catch_up(&scope, Options::default()).unwrap();
    assert!(matches!(done, CatchUp::Current { .. }), "{done:?}");
    assert!(index.delta().is_none());
}

#[test]
fn an_edit_a_new_folder_and_a_save_by_rename_are_picked_up() {
    let tree = tempfile::tempdir().unwrap();
    let root = tree.path();
    fs::write(root.join("a.txt"), "old").unwrap();
    fs::write(root.join("b.txt"), "first").unwrap();
    let cache = tempfile::tempdir().unwrap();
    let scope = scope(root, 0);
    let (mut index, _) =
        Index::build(&scope, Options::default(), &cache.path().join("idx")).unwrap();

    fs::write(root.join("a.txt"), "new").unwrap();
    fs::create_dir_all(root.join("sub/deeper")).unwrap();
    fs::write(root.join("sub/deeper/c.txt"), "made later").unwrap();
    // An editor's safe save: write a temporary file, then rename it over the original.
    fs::write(root.join("b.txt.tmp"), "second").unwrap();
    fs::rename(root.join("b.txt.tmp"), root.join("b.txt")).unwrap();

    let done = index.catch_up(&scope, Options::default()).unwrap();
    assert!(matches!(done, CatchUp::Updated { .. }), "{done:?}");
    let files = view(&index).files;
    let get = |p: &str| files.get(&(0, PathBuf::from(p))).and_then(|f| f.0.clone());
    assert_eq!(get("a.txt").as_deref(), Some(&b"new"[..]));
    assert_eq!(get("b.txt").as_deref(), Some(&b"second"[..]));
    assert_eq!(
        get(r"sub\deeper\c.txt").as_deref(),
        Some(&b"made later"[..])
    );
    assert_eq!(files.len(), 3);

    // The delta survives reopening.
    let key = mtseek_index::scope_key(&scope).unwrap();
    let reopened = Index::open(&cache.path().join("idx"), &key).unwrap();
    assert_eq!(view(&reopened), view(&index));
}

/// `.git\info\exclude` is honoured like a `.gitignore`: creating, editing and deleting it is
/// picked up by catch-up without a rebuild (ADR 0012).
#[test]
fn git_exclude_changes_are_picked_up() {
    let tree = tempfile::tempdir().unwrap();
    let root = tree.path().join("root");
    fs::create_dir_all(root.join(".git").join("info")).unwrap();
    fs::write(root.join("x.txt"), "x").unwrap();
    fs::write(root.join("y.rs"), "y").unwrap();
    let cache = tempfile::tempdir().unwrap();
    let scope = scope(&root, 0);
    let (mut index, _) =
        Index::build(&scope, Options::default(), &cache.path().join("idx")).unwrap();
    // The files the walk lists; what it leaves out is held by name too (ADR 0016).
    let names = |index: &Index| -> Vec<PathBuf> {
        view(index)
            .files
            .into_iter()
            .filter(|(_, held)| held.1 & membership::FILTERS != 0)
            .map(|((_, p), _)| p)
            .collect()
    };
    let exclude = root.join(".git").join("info").join("exclude");
    for (text, expected) in [
        (Some("x.txt\n"), vec!["y.rs"]),
        (Some("y.rs\n"), vec!["x.txt"]),
        (None, vec!["x.txt", "y.rs"]),
    ] {
        match text {
            Some(text) => fs::write(&exclude, text).unwrap(),
            None => fs::remove_file(&exclude).unwrap(),
        }
        let done = index.catch_up(&scope, Options::default()).unwrap();
        assert!(matches!(done, CatchUp::Updated { .. }), "{done:?}");
        let expected: Vec<PathBuf> = expected.into_iter().map(PathBuf::from).collect();
        assert_eq!(names(&index), expected, "after {text:?}");
    }
}

/// An ignore file appearing above the indexed folder changes what the walk lists, so
/// catch-up asks for a rebuild (ADR 0012).
#[test]
fn an_ignore_file_above_the_root_asks_for_a_rebuild() {
    let tree = tempfile::tempdir().unwrap();
    let root = tree.path().join("root");
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("x.txt"), "x").unwrap();
    let cache = tempfile::tempdir().unwrap();
    let scope = scope(&root, 0);
    let (mut index, _) =
        Index::build(&scope, Options::default(), &cache.path().join("idx")).unwrap();
    fs::write(tree.path().join(".ignore"), "x.txt\n").unwrap();
    let done = index.catch_up(&scope, Options::default()).unwrap();
    assert!(matches!(done, CatchUp::NeedsRebuild(_)), "{done:?}");
}

/// A permission change alone doesn't make catch-up read a file again: it records the new
/// descriptor and keeps the bytes, which searches then hide from whoever may no longer read
/// them (ADR 0014).
#[test]
fn a_permission_change_records_the_descriptor_and_keeps_the_bytes() {
    let tree = tempfile::tempdir().unwrap();
    let root = tree.path();
    let path = root.join("a.txt");
    fs::write(&path, "kept").unwrap();
    let cache = tempfile::tempdir().unwrap();
    let scope = scope(root, 0);
    let (mut index, _) =
        Index::build(&scope, Options::default(), &cache.path().join("idx")).unwrap();
    let before = view(&index).files[&(0, PathBuf::from("a.txt"))].clone();

    let sid = mtseek_ntfs::pipe::user_sid().unwrap();
    mtseek_ntfs::security::set_dacl(&path, &format!("D:P(D;;0x1;;;{sid})(A;;FA;;;{sid})")).unwrap();
    let done = index.catch_up(&scope, Options::default()).unwrap();
    let after = view(&index).files[&(0, PathBuf::from("a.txt"))].clone();
    mtseek_ntfs::security::set_dacl(&path, &format!("D:P(A;;FA;;;{sid})")).unwrap();

    let CatchUp::Updated { read, .. } = done else {
        panic!("{done:?}");
    };
    assert_eq!(read, 0, "nothing read again");
    assert_eq!(after.0.as_deref(), Some(&b"kept"[..]));
    assert_ne!(after.2, before.2, "the new descriptor");
    let token = mtseek_ntfs::security::Token::current().unwrap();
    assert!(token.allows(&before.2, mtseek_ntfs::security::READ));
    assert!(!token.allows(&after.2, mtseek_ntfs::security::READ));
}
