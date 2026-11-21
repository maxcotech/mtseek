//! The subtree rule (ADR 0012): if the walk from `C` with some filter enters folder `R`, the
//! walk from `R` with the same filter lists exactly what the walk from `C` lists under `R`.
//!
//! The content index answers a search of a subfolder from the index of a covering folder,
//! so this property is what makes that exact. It holds because a walk reads the ignore files
//! of its root's parents. This test checks it on random trees with nested `.gitignore`,
//! `.ignore`, `.rgignore` and `.git`, hidden entries and whitelists, for every folder entered
//! and every filter.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use mtseek_core::{Filter, Scope};
use mtseek_walk::Options;
use proptest::prelude::*;

const NAMES: &[&str] = &["a", "b", "src", ".h", "node_modules", "x.log", "keep.log"];
const PATTERNS: &[&str] = &[
    "*.log",
    "!keep.log",
    "/a",
    "b/",
    "node_modules/",
    "**/x*",
    "a/*",
    "!a/b",
    "src",
    "!src",
    ".h",
    "*",
    "!*/",
];
const CONTROLS: &[&str] = &[".gitignore", ".ignore", ".rgignore"];

#[derive(Debug, Clone)]
struct Tree {
    /// Relative paths of folders and files (a path is a folder if another path is under it).
    paths: Vec<Vec<usize>>,
    /// (folder path, control file, patterns).
    controls: Vec<(Vec<usize>, usize, Vec<usize>)>,
    /// Folders that get a `.git` folder.
    repos: Vec<Vec<usize>>,
}

fn component_path(parts: &[usize]) -> PathBuf {
    parts.iter().map(|&i| NAMES[i]).collect()
}

fn tree() -> impl Strategy<Value = Tree> {
    let path = || proptest::collection::vec(0..NAMES.len(), 1..4);
    let folder = || proptest::collection::vec(0..NAMES.len(), 0..3);
    (
        proptest::collection::vec(path(), 1..25),
        proptest::collection::vec(
            (
                folder(),
                0..CONTROLS.len(),
                proptest::collection::vec(0..PATTERNS.len(), 1..4),
            ),
            0..5,
        ),
        proptest::collection::vec(folder(), 0..3),
    )
        .prop_map(|(paths, controls, repos)| Tree {
            paths,
            controls,
            repos,
        })
}

/// Creates the tree under `root`. Leaves become files; everything with children is a folder.
fn create(root: &Path, tree: &Tree) {
    let mut folders: BTreeSet<PathBuf> = BTreeSet::new();
    for p in &tree.paths {
        for n in 1..p.len() {
            folders.insert(component_path(&p[..n]));
        }
    }
    for (dir, _, _) in &tree.controls {
        folders.extend((1..=dir.len()).map(|n| component_path(&dir[..n])));
    }
    for dir in &tree.repos {
        folders.extend((1..=dir.len()).map(|n| component_path(&dir[..n])));
    }
    for f in &folders {
        std::fs::create_dir_all(root.join(f)).expect("test setup");
    }
    for p in &tree.paths {
        let path = component_path(p);
        if !folders.contains(&path) {
            std::fs::write(root.join(&path), "x").expect("test setup");
        }
    }
    for (dir, control, patterns) in &tree.controls {
        let mut text = String::new();
        for &i in patterns {
            text.push_str(PATTERNS[i]);
            text.push('\n');
        }
        std::fs::write(
            root.join(component_path(dir)).join(CONTROLS[*control]),
            text,
        )
        .expect("test setup");
    }
    for dir in &tree.repos {
        std::fs::create_dir_all(root.join(component_path(dir)).join(".git")).expect("test setup");
    }
}

/// What the walk from `start` lists (files and the folders it enters), relative to `base`.
fn walk(start: &Path, base: &Path, filter: Filter) -> (BTreeSet<PathBuf>, BTreeSet<PathBuf>) {
    let out = Mutex::new((BTreeSet::new(), BTreeSet::new()));
    mtseek_walk::list(
        &Scope::new([start]).with_filter(filter),
        Options { threads: 1 },
        None,
        &|entry| {
            let rel = entry
                .path
                .strip_prefix(base)
                .expect("test setup")
                .to_path_buf();
            let mut out = out.lock().expect("test setup");
            if entry.is_dir {
                out.1.insert(rel);
            } else {
                out.0.insert(rel);
            }
        },
        &|_| {},
    );
    out.into_inner().expect("test setup")
}

fn filters() -> [Filter; 4] {
    let mut hidden = Filter::unrestricted(0);
    hidden.include_hidden = true;
    [
        Filter::unrestricted(0),
        Filter::unrestricted(1),
        hidden,
        Filter::unrestricted(2),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(
        std::env::var("PROPTEST_CASES").ok().and_then(|v| v.parse().ok()).unwrap_or(48)
    ))]

    #[test]
    fn a_walk_from_an_entered_folder_lists_what_the_parent_walk_lists_there(tree in tree()) {
        let dir = tempfile::tempdir().expect("test setup");
        // The walk also reads ignore files above its root; keep the tree in its own folder
        // so that both walks see the same parents.
        let base = dir.path().join("c");
        std::fs::create_dir(&base).expect("test setup");
        create(&base, &tree);
        for filter in filters() {
            let (files, dirs) = walk(&base, &base, filter);
            for r in &dirs {
                let (sub_files, sub_dirs) = walk(&base.join(r), &base, filter);
                let under = |set: &BTreeSet<PathBuf>| -> BTreeSet<PathBuf> {
                    set.iter().filter(|p| p.starts_with(r)).cloned().collect()
                };
                prop_assert_eq!(&sub_files, &under(&files), "files under {:?}, {:?}", r, filter);
                prop_assert_eq!(&sub_dirs, &under(&dirs), "folders under {:?}, {:?}", r, filter);
            }
        }
    }
}
