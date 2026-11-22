//! What a caller may see (ADR 0014): after a permission change, the content index still
//! holds the bytes it read before, and a search must show exactly what `rg` run by the same
//! account shows: "Access is denied" for a file it may no longer read and for a folder it
//! may no longer list, and nothing from either. In direct mode and through a daemon.
//!
//! The test takes access away from its own account, which needs no admin: an owner can
//! always change a file's DACL back.

#![cfg(windows)]
#![allow(clippy::print_stderr)] // tells the developer why the test was skipped

mod common;

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use common::How;
use mtseek_ntfs::security;

/// Restores full access to the paths it holds when dropped, so the temp dir can go.
struct Locked {
    paths: Vec<PathBuf>,
    sid: String,
}

impl Locked {
    fn new() -> Self {
        Self {
            paths: Vec::new(),
            sid: mtseek_ntfs::pipe::user_sid().expect("own SID"),
        }
    }

    /// Takes reading (a file) or listing (a folder) away from this account. Everything
    /// else stays allowed, inherited by what is inside.
    fn deny(&mut self, path: &Path) {
        let sid = &self.sid;
        security::set_dacl(path, &format!("D:P(D;;0x1;;;{sid})(A;OICI;FA;;;{sid})"))
            .expect("change the DACL");
        self.paths.push(path.to_path_buf());
    }

    fn restore(&mut self) {
        let sid = self.sid.clone();
        for path in self.paths.drain(..).rev() {
            let _ = security::set_dacl(&path, &format!("D:P(A;OICI;FA;;;{sid})"));
        }
    }
}

impl Drop for Locked {
    fn drop(&mut self) {
        self.restore();
    }
}

/// Exit code, stdout lines and stderr lines (without the program's name), sorted.
type Output = (Option<i32>, BTreeSet<String>, BTreeSet<String>);

fn run(mut command: Command, args: &[&str], root: &Path) -> Output {
    let out = command
        .args(args)
        .arg("--")
        .arg("needle")
        .arg(root)
        .output()
        .expect("runs");
    let lines = |bytes: &[u8]| -> BTreeSet<String> {
        String::from_utf8_lossy(bytes)
            .lines()
            .map(|l| {
                l.strip_prefix("rg: ")
                    .or_else(|| l.strip_prefix("mtseek: "))
                    .unwrap_or(l)
                    .to_owned()
            })
            .collect()
    };
    (out.status.code(), lines(&out.stdout), lines(&out.stderr))
}

fn ripgrep_available() -> bool {
    let found = Command::new("rg")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success());
    if !found {
        assert!(
            std::env::var_os("MTSEEK_REQUIRE_RG").is_none(),
            "MTSEEK_REQUIRE_RG is set but `rg` is not on PATH"
        );
        eprintln!("skipping: `rg` not on PATH");
    }
    found
}

#[test]
fn what_the_account_may_no_longer_open_is_shown_as_rg_shows_it() {
    if !ripgrep_available() {
        return;
    }
    for how in How::both("access") {
        lost_access(&how);
    }
}

fn lost_access(how: &How) {
    let tree = tempfile::tempdir().expect("temp dir");
    let root = tree.path();
    fs::write(root.join("open.txt"), "an open needle\n").expect("write");
    fs::write(root.join("locked.txt"), "a locked needle\n").expect("write");
    // A secret: never stored, read from disk by each search (ADR 0012 §4).
    fs::write(root.join("id_rsa"), "a secret needle\n").expect("write");
    fs::create_dir(root.join("closed")).expect("mkdir");
    fs::write(root.join("closed").join("inner.txt"), "a hidden needle\n").expect("write");
    fs::create_dir(root.join("fine")).expect("mkdir");
    fs::write(root.join("fine").join("x.txt"), "a fine needle\n").expect("write");
    let args = ["--line-number", "--no-heading", "--with-filename"];

    // Build the index while everything is readable.
    let mtseek = || how.command(env!("CARGO_BIN_EXE_mtseek"));
    let mut first = mtseek();
    first.arg("--index");
    let (code, built, _) = run(first, &args, root);
    assert_eq!(code, Some(0), "{}", how.label());
    assert_eq!(built.len(), 5, "{}: {built:?}", how.label());

    let mut locked = Locked::new();
    locked.deny(&root.join("locked.txt"));
    locked.deny(&root.join("closed"));
    let expected = run(
        Command::new("rg"),
        &[&["--no-config"][..], &args].concat(),
        root,
    );
    assert_eq!(expected.0, Some(2), "rg reports what it can't open");
    let mut with_stats = mtseek();
    with_stats.arg("--stats");
    let (code, out, err) = run(with_stats, &args, root);
    let errors: BTreeSet<String> = err
        .iter()
        .filter(|l| l.contains("os error"))
        .cloned()
        .collect();
    assert!(
        err.iter().any(|l| l.contains("candidates")),
        "{}: answered from the index: {err:?}",
        how.label()
    );
    assert_eq!(
        (code, &out, &errors),
        (expected.0, &expected.1, &expected.2),
        "{}",
        how.label()
    );
    assert!(
        !out.iter()
            .any(|l| l.contains("locked") || l.contains("hidden")),
        "{}: {out:?}",
        how.label()
    );

    // Given back, both are searched again.
    locked.restore();
    let (code, out, _) = run(mtseek(), &args, root);
    assert_eq!((code, out), (Some(0), built), "{}", how.label());
}
