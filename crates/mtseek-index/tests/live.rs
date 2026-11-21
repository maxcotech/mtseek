//! Invariant 3 for the content index (ADR 0011): after any sequence of changes, an index
//! caught up from the change journal holds exactly what a fresh build holds. Same files,
//! same stored bytes, same directories, control files and errors.
//!
//! Windows only: the journal is NTFS's. Needs no admin rights.

#![cfg(windows)]

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use mtseek_core::{Filter, Scope};
use mtseek_index::{CatchUp, Index, Location};
use mtseek_walk::Options;
use proptest::prelude::*;

/// Everything an index answers with, keyed by (root, relative path).
#[derive(Debug, PartialEq, Eq)]
struct View {
    files: BTreeMap<(usize, PathBuf), Option<Vec<u8>>>,
    dirs: Vec<(u32, PathBuf)>,
    controls: Vec<(u32, PathBuf)>,
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
            let previous = files.insert(segment.path(id), bytes);
            assert!(
                previous.is_none(),
                "{:?} is in two layers",
                segment.path(id)
            );
        }
    }
    let newest = index.delta().unwrap_or_else(|| index.base());
    let paths = |entries: Vec<mtseek_index::Entry>| {
        let mut out: Vec<(u32, PathBuf)> = entries.into_iter().map(|e| (e.root, e.path)).collect();
        out.sort();
        out
    };
    let mut errors: Vec<String> = index
        .errors()
        .iter()
        .map(|e| format!("{:?} {}: {}", e.root, e.path.display(), e.message))
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
        1 => (d, f, any::<bool>()).prop_map(|(dir, file, hidden)| Op::Hide { dir, file, hidden }),
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
            if dir(d).is_dir() {
                let _ = fs::create_dir(dir(d).join(".git"));
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
    }
}

fn scope(root: &Path, level: u8) -> Scope {
    Scope::new([root]).with_filter(Filter::unrestricted(level))
}

/// Catches `index` up and checks it against a fresh build of the same scope.
fn check(index: &mut Index, scope: &Scope, cache: &Path) -> Result<(), TestCaseError> {
    let done = index
        .catch_up(scope, Options::default())
        .expect("catch-up works");
    prop_assert!(!matches!(done, CatchUp::NeedsRebuild(_)), "{done:?}");
    let (fresh, _) =
        Index::build(scope, Options::default(), &cache.join("fresh")).expect("fresh build");
    let (caught_up, expected) = (view(index), view(&fresh));
    prop_assert_eq!(caught_up, expected, "after {:?}", done);
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    #[test]
    fn a_caught_up_index_equals_a_fresh_build(
        setup in proptest::collection::vec(op(), 0..12),
        batches in proptest::collection::vec(proptest::collection::vec(op(), 1..8), 1..4),
        level in 0u8..=3,
    ) {
        let tree = tempfile::tempdir().expect("temp dir");
        let cache = tempfile::tempdir().expect("temp dir");
        for op in &setup {
            apply(tree.path(), op);
        }
        let scope = scope(tree.path(), level);
        let (mut index, _) =
            Index::build(&scope, Options::default(), &cache.path().join("idx")).expect("build");
        for batch in &batches {
            for op in batch {
                apply(tree.path(), op);
            }
            check(&mut index, &scope, cache.path())?;
        }
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
    let get = |p: &str| files.get(&(0, PathBuf::from(p))).cloned().flatten();
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
