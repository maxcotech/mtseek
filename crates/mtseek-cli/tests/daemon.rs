//! `mtseek` through the daemon (ADR 0013): the same bytes on stdout and stderr and the same
//! exit codes as direct mode, a silent fallback when no daemon runs, and early stops.

#![cfg(windows)]

mod common;

use std::ffi::OsString;
use std::io::{BufRead, BufReader};
use std::os::windows::ffi::OsStringExt;
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use common::{How, NO_DAEMON};

fn mtseek(how: &How, cwd: &Path, args: &[&str]) -> Output {
    how.command(env!("CARGO_BIN_EXE_mtseek"))
        .current_dir(cwd)
        .args(args)
        .output()
        .expect("mtseek runs")
}

/// A tree with a non-UTF-8 line, a file and a folder whose names aren't valid Unicode,
/// and a binary file that matches before its NUL.
fn odd_tree() -> tempfile::TempDir {
    let tree = tempfile::tempdir().expect("temp dir");
    let root = tree.path();
    std::fs::write(root.join("latin1.txt"), b"caf\xe9 needle\r\nplain needle\n")
        .expect("write the tree");
    let odd = OsString::from_wide(&[0x6F, 0xD800, 0x64]); // "o", unpaired surrogate, "d"
    std::fs::create_dir(root.join(&odd)).expect("write the tree");
    std::fs::write(root.join(&odd).join("inner.txt"), "needle inside\n").expect("write the tree");
    let mut odd_file = odd.clone();
    odd_file.push(".txt");
    std::fs::write(root.join(&odd_file), "needle in a file\n").expect("write the tree");
    let mut binary = b"needle early\n".to_vec();
    binary.extend(std::iter::repeat_n(b'x', 300_000)); // the NUL comes after the first buffer
    binary.push(0);
    std::fs::write(root.join("late.bin"), binary).expect("write the tree");
    std::fs::create_dir(root.join("sub")).expect("write the tree");
    std::fs::write(root.join("sub").join("s.txt"), "needle below\n").expect("write the tree");
    tree
}

/// Output lines, sorted: the order files are searched in isn't fixed.
fn sorted(bytes: &[u8]) -> Vec<Vec<u8>> {
    let mut lines: Vec<Vec<u8>> = bytes
        .split_inclusive(|&b| b == b'\n')
        .map(<[u8]>::to_vec)
        .collect();
    lines.sort();
    lines
}

#[test]
fn the_daemon_prints_exactly_what_direct_mode_prints() {
    let tree = odd_tree();
    let root = tree.path();
    let root_text = root.to_str().unwrap();
    let direct = How::direct();
    let daemon = How::daemon("exact");
    let cases: &[&[&str]] = &[
        &["-n", "-H", "needle"],
        &["-n", "-H", "needle", "."],
        &["-n", "-H", "needle", "sub", "."],
        &["-l", "needle", root_text],
        &["-n", "-H", "--index", "needle", "."],
        &["-n", "-H", "needle", "."],
        &["-n", "-H", "-uuu", "needle", "."],
        &["-n", "-H", "needle", "missing-folder", "."],
        &["-n", "-H", "needle", "sub/s.txt"],
        &["-n", "-H", "needle", "."],
        &["-n", "-H", "(unclosed", "."],
        &["-n", "-H", "absent-zq7xk", "."],
    ];
    for args in cases {
        let want = mtseek(&direct, root, args);
        let got = mtseek(&daemon, root, args);
        assert_eq!(got.status.code(), want.status.code(), "{args:?}");
        assert_eq!(sorted(&got.stdout), sorted(&want.stdout), "{args:?}");
        assert_eq!(
            String::from_utf8_lossy(&got.stderr),
            String::from_utf8_lossy(&want.stderr),
            "{args:?}"
        );
    }
    // The cases above found the odd names, the Latin-1 line and the binary warning.
    let all = mtseek(&daemon, root, &["-n", "-H", "needle", "."]).stdout;
    let text = String::from_utf8_lossy(&all);
    assert!(text.contains("inner.txt"), "{text}");
    assert!(all.windows(4).any(|w| w == b"caf\xe9"), "raw bytes survive");
    assert!(
        text.contains("WARNING: stopped searching binary file"),
        "{text}"
    );
}

#[test]
fn without_a_daemon_the_cli_searches_itself_unless_told_not_to() {
    let tree = odd_tree();
    let cwd = tree.path();
    let plain = Command::new(env!("CARGO_BIN_EXE_mtseek"))
        .env("MTSEEK_PIPE", NO_DAEMON)
        .current_dir(cwd)
        .args(["--stats", "-l", "needle", "sub"])
        .output()
        .unwrap();
    assert_eq!(plain.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&plain.stderr).contains("answered in this process"));

    let required = Command::new(env!("CARGO_BIN_EXE_mtseek"))
        .env("MTSEEK_PIPE", NO_DAEMON)
        .current_dir(cwd)
        .args(["--daemon", "-l", "needle", "sub"])
        .output()
        .unwrap();
    assert_eq!(required.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&required.stderr).contains("no daemon"));
    assert_eq!(required.stdout, b"");

    // --no-daemon wins over a running daemon.
    let daemon = How::daemon("bypass");
    let bypassed = daemon
        .command(env!("CARGO_BIN_EXE_mtseek"))
        .current_dir(cwd)
        .args(["--stats", "-l", "needle", "sub"])
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&bypassed.stderr).contains("answered by mtseekd"));
    let mut direct = Command::new(env!("CARGO_BIN_EXE_mtseek"));
    let bypassed = direct
        .env("MTSEEK_PIPE", daemon_pipe(&daemon))
        .current_dir(cwd)
        .args(["--no-daemon", "--stats", "-l", "needle", "sub"])
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&bypassed.stderr).contains("answered in this process"));
}

/// The pipe of a daemon `How`, read back from the command it builds.
fn daemon_pipe(how: &How) -> OsString {
    how.command("x")
        .get_envs()
        .find(|(k, _)| *k == "MTSEEK_PIPE")
        .and_then(|(_, v)| v.map(ToOwned::to_owned))
        .expect("a daemon pipe")
}

#[test]
fn a_reader_that_stops_early_stops_the_search_and_the_daemon_carries_on() {
    let tree = tempfile::tempdir().unwrap();
    for i in 0..3000 {
        std::fs::write(
            tree.path().join(format!("f{i:04}.txt")),
            "needle\n".repeat(20),
        )
        .unwrap();
    }
    let daemon = How::daemon("head");
    let mut child = daemon
        .command(env!("CARGO_BIN_EXE_mtseek"))
        .current_dir(tree.path())
        .args(["-n", "needle"])
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut first = String::new();
    BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut first)
        .unwrap(); // the reader is dropped here, like `| head -1`
    assert!(first.contains("needle"), "{first}");
    let started = Instant::now();
    let status = child.wait().unwrap();
    assert!(
        status.success(),
        "a closed pipe is a normal stop: {status:?}"
    );
    assert!(started.elapsed() < Duration::from_secs(20));

    let again = mtseek(&daemon, tree.path(), &["-l", "needle", "f0001.txt"]);
    assert_eq!(again.status.code(), Some(0));
    assert_eq!(String::from_utf8_lossy(&again.stdout).trim(), "f0001.txt");
}
