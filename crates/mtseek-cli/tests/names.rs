//! `mtseek --names`: completeness against `rg --files -uu`, plus query behaviour.
//!
//! Runs whichever engine the test process can use: the NTFS MFT when elevated (CI),
//! the parallel directory walk otherwise. Both must give the same answers.

#![allow(clippy::print_stderr)] // tells the developer why the rg test was skipped

use std::collections::BTreeSet;
use std::path::Path;
use std::process::Command;

use mtseek_testkit::CorpusSpec;

fn mtseek(args: &[&str], root: &Path) -> (Option<i32>, Vec<String>) {
    let out = Command::new(env!("CARGO_BIN_EXE_mtseek"))
        .arg("--names")
        .args(args)
        .arg("--")
        .arg(root)
        .output()
        .expect("mtseek runs");
    let lines = String::from_utf8(out.stdout)
        .expect("paths are UTF-8")
        .lines()
        .map(str::to_owned)
        .collect();
    (out.status.code(), lines)
}

fn names(lines: &[String]) -> Vec<String> {
    lines
        .iter()
        .map(|l| {
            Path::new(l)
                .file_name()
                .expect("every result has a file name")
                .to_string_lossy()
                .into_owned()
        })
        .collect()
}

#[test]
fn lists_every_file_rg_lists_plus_directories() {
    let found = Command::new("rg")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success());
    if !found {
        assert!(
            std::env::var_os("MTSEEK_REQUIRE_RG").is_none(),
            "MTSEEK_REQUIRE_RG set, rg missing"
        );
        eprintln!("skipping: `rg` not on PATH");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    mtseek_testkit::generate(dir.path(), &CorpusSpec::default()).unwrap();

    let rg = Command::new("rg")
        .args(["--no-config", "--files", "-uu"])
        .arg(dir.path())
        .output()
        .unwrap();
    let rg: BTreeSet<String> = String::from_utf8(rg.stdout)
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect();
    let (code, listed) = mtseek(&[""], dir.path());
    assert_eq!(code, Some(0));
    let listed: BTreeSet<String> = listed.into_iter().collect();

    let missing: Vec<_> = rg.difference(&listed).collect();
    assert!(
        missing.is_empty(),
        "files rg lists but mtseek does not: {missing:?}"
    );
    let extra_files: Vec<_> = listed
        .difference(&rg)
        .filter(|p| !Path::new(p).is_dir())
        .collect();
    assert!(
        extra_files.is_empty(),
        "non-directories mtseek lists but rg does not: {extra_files:?}"
    );
}

#[test]
fn terms_case_and_exit_codes() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join("src/Payments")).unwrap();
    std::fs::write(root.join("src/Payments/PaystackClient.cs"), "").unwrap();
    std::fs::write(root.join("src/paystack.json"), "").unwrap();
    std::fs::write(root.join("README.md"), "").unwrap();

    let (code, out) = mtseek(&["paystack"], root);
    assert_eq!(code, Some(0));
    assert_eq!(
        names(&out),
        ["PaystackClient.cs", "paystack.json"],
        "smart case: lowercase matches both"
    );

    let (_, out) = mtseek(&["Paystack"], root);
    assert_eq!(
        names(&out),
        ["PaystackClient.cs"],
        "smart case: uppercase is exact"
    );

    let (_, out) = mtseek(&["-i", "Paystack"], root);
    assert_eq!(out.len(), 2);

    let (_, out) = mtseek(&["pay .json"], root);
    assert_eq!(names(&out), ["paystack.json"], "every term must match");

    let (_, out) = mtseek(&["payments"], root);
    assert_eq!(
        out,
        [root.join("src").join("Payments").to_string_lossy()],
        "directories match too"
    );

    let (code, out) = mtseek(&["zq7xkvq9plmw"], root);
    assert_eq!((code, out.len()), (Some(1), 0), "no match exits 1, like rg");
}

#[test]
fn content_only_flags_are_rejected_with_names() {
    let out = Command::new(env!("CARGO_BIN_EXE_mtseek"))
        .args(["--names", "-l", "x", "."])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("cannot be used with"));
}
