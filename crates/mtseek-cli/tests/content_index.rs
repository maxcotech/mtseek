//! The content index from the command line (ADR 0011): `--index` builds it, later searches
//! use it without asking and see every change made in between, `--no-index` scans.

use std::fs;
use std::path::Path;
use std::process::Command;

/// Runs `mtseek --stats -l <args> needle <root>` with the given cache. Returns exit code,
/// matching file names (sorted) and stderr.
fn mtseek(cache: &Path, args: &[&str], root: &Path) -> (Option<i32>, Vec<String>, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_mtseek"))
        .env("MTSEEK_CACHE_DIR", cache)
        .arg("--stats")
        .arg("-l")
        .args(args)
        .arg("--")
        .arg("needle")
        .arg(root)
        .output()
        .expect("mtseek runs");
    let mut names: Vec<String> = String::from_utf8(out.stdout)
        .expect("paths are UTF-8")
        .lines()
        .map(|l| {
            Path::new(l)
                .strip_prefix(root)
                .expect("paths are under the root")
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    names.sort();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    (out.status.code(), names, stderr)
}

#[test]
fn an_indexed_scope_is_searched_through_its_index_and_stays_current() {
    let tree = tempfile::tempdir().expect("temp dir");
    let cache = tempfile::tempdir().expect("temp dir");
    let root = tree.path();
    fs::write(root.join("a.txt"), "a needle\n").expect("write");
    fs::write(root.join("b.txt"), "nothing\n").expect("write");

    // No index yet: a plain search scans and builds nothing.
    let (code, names, stderr) = mtseek(cache.path(), &[], root);
    assert_eq!((code, names), (Some(0), vec!["a.txt".to_owned()]));
    assert!(!stderr.contains("index"), "{stderr}");

    // --index builds it.
    let (code, names, stderr) = mtseek(cache.path(), &["--index"], root);
    assert_eq!((code, names), (Some(0), vec!["a.txt".to_owned()]));
    assert!(stderr.contains("built"), "{stderr}");

    // Changes between runs are applied before the next search, which needs no flag.
    fs::write(root.join("a.txt"), "no longer\n").expect("write");
    fs::write(root.join("b.txt"), "now a needle\n").expect("write");
    fs::create_dir(root.join("new")).expect("mkdir");
    fs::write(root.join("new").join("c.txt"), "needle too\n").expect("write");
    let (code, names, stderr) = mtseek(cache.path(), &[], root);
    assert_eq!(
        (code, names),
        (
            Some(0),
            vec![
                "b.txt".to_owned(),
                format!("new{}c.txt", std::path::MAIN_SEPARATOR)
            ]
        )
    );
    if cfg!(windows) {
        assert!(stderr.contains("caught up"), "{stderr}");
    }

    // --no-index scans, and agrees.
    let (code, names, stderr) = mtseek(cache.path(), &["--no-index"], root);
    assert_eq!(code, Some(0));
    assert_eq!(names.len(), 2);
    assert!(!stderr.contains("index"), "{stderr}");

    // Deleting every match is seen too: exit code 1, like rg.
    fs::remove_file(root.join("b.txt")).expect("delete");
    fs::remove_dir_all(root.join("new")).expect("delete");
    let (code, names, _) = mtseek(cache.path(), &[], root);
    assert_eq!((code, names), (Some(1), Vec::<String>::new()));
}
