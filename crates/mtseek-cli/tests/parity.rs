//! Parity with ripgrep: the contract in `docs/adr/0005-ripgrep-compatible-contract.md`.
//!
//! Generates a synthetic corpus full of edge cases, runs the same query through `rg` and
//! `mtseek` with the same flags, and requires identical output (order-insensitive).
//!
//! Needs `rg` on PATH. Without it the test is skipped, unless `MTSEEK_REQUIRE_RG=1`
//! (set in CI) turns the skip into a failure.

#![allow(clippy::print_stderr)] // tells the developer why the test was skipped

mod common;

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::Path;
use std::process::Command;

use common::How;
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

/// mtseek runs every case scanning and through the content index (`--index`), each in its
/// own process and through a daemon (ADR 0013).
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
    let ways = How::both("parity");
    let rg = How::direct();

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
                let expected = run(&rg, "rg", &["--no-config"], &flags, pattern, dir.path());
                for (how, variant) in ways
                    .iter()
                    .flat_map(|how| MTSEEK_VARIANTS.iter().map(move |v| (how, v)))
                {
                    let actual = run(
                        how,
                        env!("CARGO_BIN_EXE_mtseek"),
                        variant,
                        &flags,
                        pattern,
                        dir.path(),
                    );
                    cases += 1;
                    if expected != actual {
                        let all_flags: Vec<&str> = [how.label()]
                            .into_iter()
                            .chain(variant.iter().copied())
                            .chain(flags.iter().copied())
                            .collect();
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

/// Searches of subfolders, and of other spellings of a path, answered from the index of a
/// covering folder (ADR 0012), match rg on the same paths.
#[test]
fn subfolder_searches_through_a_parent_index_match_ripgrep() {
    if !ripgrep_available() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    mtseek_testkit::generate(dir.path(), &CorpusSpec::default()).unwrap();
    let ways = How::both("subfolders");
    let rg = How::direct();
    // Index the whole corpus with the widest filters, so that every search below can use it.
    for how in &ways {
        run(
            how,
            env!("CARGO_BIN_EXE_mtseek"),
            &["--index", "-uu", "-l"],
            &[],
            tokens::ABSENT,
            dir.path(),
        );
    }
    let mut roots: Vec<std::path::PathBuf> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.is_dir())
        .collect();
    roots.sort();
    roots.push(dir.path().join("edge").join(".hidden-dir"));
    // The same folder typed in another case is the same index entry, printed as typed.
    let upper = std::path::PathBuf::from(dir.path().to_string_lossy().to_uppercase());
    roots.push(upper.join("edge"));

    let mut failures = Vec::new();
    let mut cases = 0;
    for root in &roots {
        for (pattern, query_flags) in &QUERIES[..6] {
            for scope_flags in [&[][..], &["-u"], &["-uu"], &["--hidden"]] {
                for mode in [FILES_MODE, LINES_MODE] {
                    let flags: Vec<&str> = query_flags
                        .iter()
                        .chain(scope_flags)
                        .chain(mode)
                        .copied()
                        .collect();
                    let expected = run(&rg, "rg", &["--no-config"], &flags, pattern, root);
                    for how in &ways {
                        let actual = run(
                            how,
                            env!("CARGO_BIN_EXE_mtseek"),
                            &[],
                            &flags,
                            pattern,
                            root,
                        );
                        cases += 1;
                        if expected != actual {
                            let mut all = vec![how.label()];
                            all.extend(&flags);
                            let shown = root.to_string_lossy();
                            all.push(&shown);
                            failures.push(describe(pattern, &all, &expected, &actual));
                        }
                    }
                }
            }
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {cases} cases differ from rg:

{}",
        failures.len(),
        failures.join(
            "
"
        )
    );
}

#[derive(Debug, PartialEq, Eq)]
struct Outcome {
    code: Option<i32>,
    /// Output line -> how many times it appeared. Order-insensitive, duplicate-sensitive.
    lines: BTreeMap<Vec<u8>, usize>,
}

fn run(how: &How, exe: &str, pre: &[&str], flags: &[&str], pattern: &str, root: &Path) -> Outcome {
    let mut command = if exe == "rg" {
        Command::new(exe)
    } else {
        how.command(exe)
    };
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
