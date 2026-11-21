//! Parity with ripgrep: the contract in `docs/adr/0005-ripgrep-compatible-contract.md`.
//!
//! Generates a synthetic corpus full of edge cases, runs the same query through `rg` and
//! `mtseek` with the same flags, and requires identical output (order-insensitive).
//!
//! Needs `rg` on PATH. Without it the test is skipped, unless `MTSEEK_REQUIRE_RG=1`
//! (set in CI) turns the skip into a failure.

#![allow(clippy::print_stderr)] // tells the developer why the test was skipped

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::Path;
use std::process::Command;

use mtseek_testkit::{CorpusSpec, tokens};

const QUERIES: &[(&str, &[&str])] = &[
    (tokens::ABSENT, &["-F"]),
    (tokens::RARE, &["-F"]),
    (tokens::UNCOMMON, &["-F"]),
    (tokens::MEDIUM, &["-F"]),
    (tokens::COMMON, &["-F"]),
    (tokens::NEEDLE, &["-F"]),
    ("paystack", &["-F", "-i"]),
    ("paystack", &["-F", "-S"]),
    ("café", &["-F", "-i"]),
    ("straße", &["-i"]),
    (r"Connection\w*String", &[]),
    (r"\b\d{3}-\d{3}-\d{4}\b", &[]),
    (r"^const x = \w+\.call\(\);$", &[]),
    ("$5.00 (", &["-F"]),
];

/// Files whose ripgrep output is not reproducible, so they are left out of the comparison.
///
/// `binary-late.bin` (a match, then a NUL ~200 KiB later): `rg` sometimes skips it entirely
/// and sometimes reports the match plus the binary warning, flipping between runs. Each rg
/// thread reuses one searcher, whose line buffer grows to fit `long-line.txt` and is never
/// shrunk. A thread that searched that file first reads the NUL in its first buffer fill
/// and quits before searching the match (`grep-searcher`'s `ReadByLine::fill`). Searched
/// alone, rg always reports the match. mtseek gives every such file a fresh buffer, so it
/// always gives the searched-alone answer: pinned by `mtseek-walk`'s
/// `a_long_line_earlier_does_not_hide_a_match_before_a_late_nul` and
/// `binary_after_match_reports_lines_and_offset` unit tests.
const UNSTABLE: &[&str] = &["binary-late.bin"];

/// mtseek runs every case twice: scanning, and through the content index (`--index`).
const MTSEEK_VARIANTS: &[&[&str]] = &[&["--no-index"], &["--index"]];

const SCOPES: &[&[&str]] = &[&[], &["-u"], &["-uu"]];
const FILES_MODE: &[&str] = &["--files-with-matches"];
const LINES_MODE: &[&str] = &["--line-number", "--no-heading", "--with-filename"];

#[test]
fn output_matches_ripgrep() {
    if !ripgrep_available() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    mtseek_testkit::generate(dir.path(), &CorpusSpec::default()).unwrap();
    let cache = tempfile::tempdir().unwrap();

    let mut failures = Vec::new();
    let mut cases = 0;
    for (pattern, query_flags) in QUERIES {
        for scope_flags in SCOPES {
            for mode in [FILES_MODE, LINES_MODE] {
                let flags: Vec<&str> = query_flags
                    .iter()
                    .chain(*scope_flags)
                    .chain(mode)
                    .copied()
                    .collect();
                let expected = run("rg", &["--no-config"], &flags, pattern, dir.path(), None);
                for variant in MTSEEK_VARIANTS {
                    let actual = run(
                        env!("CARGO_BIN_EXE_mtseek"),
                        variant,
                        &flags,
                        pattern,
                        dir.path(),
                        Some(cache.path()),
                    );
                    cases += 1;
                    if expected != actual {
                        let all_flags: Vec<&str> = variant.iter().chain(&flags).copied().collect();
                        failures.push(describe(pattern, &all_flags, &expected, &actual));
                    }
                }
            }
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {cases} cases differ from rg:\n\n{}",
        failures.len(),
        failures.join("\n")
    );
}

#[derive(Debug, PartialEq, Eq)]
struct Outcome {
    code: Option<i32>,
    /// Output line -> how many times it appeared. Order-insensitive, duplicate-sensitive.
    lines: BTreeMap<Vec<u8>, usize>,
}

fn run(
    exe: &str,
    pre: &[&str],
    flags: &[&str],
    pattern: &str,
    root: &Path,
    cache: Option<&Path>,
) -> Outcome {
    let mut command = Command::new(exe);
    if let Some(cache) = cache {
        command.env("MTSEEK_CACHE_DIR", cache);
    }
    let output = command
        .args(pre)
        .args(flags)
        .arg("--")
        .arg(pattern)
        .arg(root)
        .output()
        .unwrap_or_else(|e| panic!("failed to run {exe}: {e}"));
    let mut lines = BTreeMap::new();
    for line in output.stdout.split_inclusive(|&b| b == b'\n') {
        let text = String::from_utf8_lossy(line);
        if UNSTABLE.iter().any(|name| text.contains(name)) {
            continue;
        }
        *lines.entry(line.to_vec()).or_insert(0) += 1;
    }
    Outcome {
        code: output.status.code(),
        lines,
    }
}

fn describe(pattern: &str, flags: &[&str], expected: &Outcome, actual: &Outcome) -> String {
    let show = |l: &[u8]| {
        String::from_utf8_lossy(l)
            .trim_end()
            .chars()
            .take(160)
            .collect::<String>()
    };
    let mut s = format!(
        "pattern {pattern:?} flags {flags:?}: exit rg={:?} mtseek={:?}\n",
        expected.code, actual.code
    );
    for (line, n) in &expected.lines {
        if actual.lines.get(line) != Some(n) {
            _ = writeln!(s, "  only in rg     : {}", show(line));
        }
    }
    for (line, n) in &actual.lines {
        if expected.lines.get(line) != Some(n) {
            _ = writeln!(s, "  only in mtseek : {}", show(line));
        }
    }
    s
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
        eprintln!("skipping ripgrep parity test: `rg` not on PATH");
    }
    found
}
