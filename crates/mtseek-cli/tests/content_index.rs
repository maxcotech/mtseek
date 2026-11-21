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

/// ADR 0012: one index answers searches of its subfolders, other spellings of its paths and
/// narrower filters. Folders its walk skips, and searches that follow links, are scanned.
#[test]
fn a_covering_index_answers_subfolders_spellings_and_narrower_filters() {
    let tree = tempfile::tempdir().expect("temp dir");
    let cache = tempfile::tempdir().expect("temp dir");
    let root = tree.path();
    for dir in ["a", ".hid", "node"] {
        fs::create_dir(root.join(dir)).expect("mkdir");
        fs::write(root.join(dir).join("n.txt"), "needle\n").expect("write");
    }
    fs::create_dir(root.join(".git")).expect("mkdir");
    fs::write(root.join(".gitignore"), "node/\n").expect("write");
    let used = |stderr: &str| stderr.contains("candidates");

    let (code, names, stderr) = mtseek(cache.path(), &["--index", "-uu"], root);
    assert_eq!(code, Some(0));
    assert_eq!(names.len(), 3, "{names:?}");
    assert!(stderr.contains("built"), "{stderr}");

    // A subfolder, typed in another case: answered from the index, printed as typed.
    let upper = std::path::PathBuf::from(root.join("a").to_string_lossy().to_uppercase());
    let (code, names, stderr) = mtseek(cache.path(), &[], &upper);
    assert_eq!((code, names), (Some(0), vec!["n.txt".to_owned()]));
    assert!(used(&stderr), "{stderr}");

    // Narrower filters: the defaults skip the hidden folder and the ignored one.
    let (_, names, stderr) = mtseek(cache.path(), &[], root);
    assert_eq!(names, vec![format!("a{}n.txt", std::path::MAIN_SEPARATOR)]);
    assert!(used(&stderr) && !stderr.contains("built"), "{stderr}");

    // An ignored folder searched by name is searched, like rg does, by scanning it.
    let (_, names, stderr) = mtseek(cache.path(), &[], &root.join("node"));
    assert_eq!(names, vec!["n.txt".to_owned()]);
    assert!(stderr.contains("scanned instead"), "{stderr}");

    // Registering a folder inside a covering index changes nothing.
    let (_, _, stderr) = mtseek(cache.path(), &["--index"], &root.join("a"));
    assert!(used(&stderr) && !stderr.contains("built"), "{stderr}");

    // Following links never uses an index.
    let (_, names, stderr) = mtseek(cache.path(), &["-L"], root);
    assert_eq!(names.len(), 1);
    assert!(!used(&stderr), "{stderr}");
}
