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
use serde_json::Value;

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

/// The flags agents use (ADR 0015): context, `-m`, counts, globs and types. Globs with a
/// folder in them are relative to the working directory, so these run from the corpus root
/// on `.`, as an agent in a project would.
const AGENT_CASES: &[(&str, &[&str])] = &[
    (tokens::MEDIUM, &["-F", "-A1"]),
    (tokens::MEDIUM, &["-F", "-B2"]),
    (tokens::MEDIUM, &["-F", "-C1"]),
    (tokens::MEDIUM, &["-F", "-C2", "-A0"]),
    ("import", &["-F", "-m1", "-A2"]),
    ("import", &["-F", "-m0"]),
    (tokens::COMMON, &["-F", "-c"]),
    (tokens::COMMON, &["-F", "--count-matches"]),
    ("e", &["-c", "-m2"]),
    ("e", &["--count-matches", "-m1"]),
    (tokens::COMMON, &["-F", "-c", "-l"]),
    (tokens::COMMON, &["-F", "-l", "-c"]),
    (tokens::COMMON, &["-F", "-l", "-g", "*.rs"]),
    (tokens::COMMON, &["-F", "-l", "-g", "*.log"]),
    (
        tokens::COMMON,
        &["-F", "-l", "-g", "!*.md", "-g", "!app/tests"],
    ),
    (
        tokens::COMMON,
        &["-F", "-l", "-g", "app/**", "-g", "!*.json"],
    ),
    (tokens::COMMON, &["-F", "-l", "--iglob", "*.TXT"]),
    (tokens::COMMON, &["-F", "-l", "-g", ".cache", "-g", "*.rs"]),
    (tokens::COMMON, &["-F", "-l", "-t", "rust"]),
    (
        tokens::COMMON,
        &["-F", "-l", "-T", "markdown", "-T", "json"],
    ),
    (
        tokens::COMMON,
        &["-F", "-l", "-t", "txt", "-T", "txt", "-t", "rust"],
    ),
    (tokens::NEEDLE, &["-F", "-g", "*.txt", "-C1"]),
    (tokens::MEDIUM, &["-F", "-c", "-t", "rust", "-m1"]),
    (tokens::COMMON, &["-t", "nope"]),
    (tokens::COMMON, &["-g", "["]),
];

/// `--json`, compared message by message; see [`JsonOutcome`].
const JSON_CASES: &[(&str, &[&str])] = &[
    (tokens::NEEDLE, &["-F"]),
    (tokens::MEDIUM, &["-F", "-A1", "-B1"]),
    ("import", &["-F", "-m1", "-A2"]),
    ("paystack", &["-F", "-i"]),
    ("café", &["-F", "-i"]),
    (r"Connection\w*String", &[]),
    (tokens::NEEDLE, &["-F", "-g", "*.log"]),
    (tokens::NEEDLE, &["-F", "-c", "--json"]),
    (tokens::NEEDLE, &["-F", "--json", "-c"]),
];

#[test]
fn agent_flags_match_ripgrep() {
    if !ripgrep_available() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    mtseek_testkit::generate(dir.path(), &CorpusSpec::default()).unwrap();
    let ways = How::both("agents");
    let rg = How::direct();
    let corpus = Some(dir.path());
    let root = Path::new(".");

    let mut failures = Vec::new();
    let mut cases = 0;
    // The last round searches with the default filters through the index the `-uu` round
    // widened, which then holds the ignored and hidden files globs can list.
    for scope_flags in [&[][..], &["-uu"], &[]] {
        for (pattern, case_flags) in AGENT_CASES {
            let flags: Vec<&str> = case_flags
                .iter()
                .chain(scope_flags)
                .chain(LINES_MODE)
                .copied()
                .collect();
            let expected = run_in(&rg, "rg", &["--no-config"], &flags, pattern, root, corpus);
            for (how, variant) in ways
                .iter()
                .flat_map(|how| MTSEEK_VARIANTS.iter().map(move |v| (how, v)))
            {
                let exe = env!("CARGO_BIN_EXE_mtseek");
                let actual = run_in(how, exe, variant, &flags, pattern, root, corpus);
                cases += 1;
                if expected != actual {
                    let mut all = vec![how.label()];
                    all.extend(variant.iter().copied());
                    all.extend(&flags);
                    failures.push(describe(pattern, &all, &expected, &actual));
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

/// Finer control over ignore files: `--no-ignore-vcs` and its siblings, `--ignore-file`
/// (which applies even with `-u`), `--no-ignore-files`, and an ignore file that can't be
/// read (reported, not an error). No index answers these walks; with `--index` they must
/// still scan rather than use the corpus's index.
#[test]
fn ignore_flags_match_ripgrep() {
    if !ripgrep_available() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    mtseek_testkit::generate(dir.path(), &CorpusSpec::default()).unwrap();
    std::fs::create_dir_all(dir.path().join("app/.git/info")).unwrap();
    std::fs::write(dir.path().join("app/.git/info/exclude"), "*.json\n").unwrap();
    let extra_dir = tempfile::tempdir().unwrap();
    let extra = extra_dir.path().join("extra.ignore");
    std::fs::write(&extra, "*.md\ndocs/\n").unwrap();
    let extra = extra.to_str().unwrap();
    let ways = How::both("ignores");
    let rg = How::direct();
    let corpus = Some(dir.path());

    let cases: &[(&[&str], &str)] = &[
        (&[], "."),
        (&["--no-ignore-vcs"], "."),
        (&["--no-ignore-dot"], "."),
        (&["--no-ignore-exclude"], "."),
        (&["--no-ignore-global"], "."),
        (&["--no-ignore-vcs", "--no-ignore-dot"], "."),
        (&[], "app/src"),
        (&["--no-ignore-parent"], "app/src"),
        (&["--ignore-file", extra], "."),
        (&["--ignore-file", extra, "--no-ignore-files"], "."),
        (&["--ignore-file", extra, "-u"], "."),
        (&["--ignore-file", extra, "-uu"], "."),
        (&["--no-ignore-vcs", "--ignore-file", extra], "."),
        (&["--ignore-file", "missing.ignore"], "."),
        (&["--no-ignore-vcs", "-g", "*.log"], "."),
        (&["--no-ignore-dot", "-c"], "."),
    ];
    let mut failures = Vec::new();
    let mut total = 0;
    for &(case_flags, root) in cases {
        let mut flags = vec!["-F", "-l"];
        flags.extend(case_flags);
        flags.extend(LINES_MODE);
        let root = Path::new(root);
        let expected = run_in(
            &rg,
            "rg",
            &["--no-config"],
            &flags,
            tokens::COMMON,
            root,
            corpus,
        );
        for (how, variant) in ways
            .iter()
            .flat_map(|how| MTSEEK_VARIANTS.iter().map(move |v| (how, v)))
        {
            let exe = env!("CARGO_BIN_EXE_mtseek");
            let actual = run_in(how, exe, variant, &flags, tokens::COMMON, root, corpus);
            total += 1;
            if expected != actual {
                let mut all = vec![how.label()];
                all.extend(variant.iter().copied());
                all.extend(&flags);
                let shown = root.to_string_lossy();
                all.push(&shown);
                failures.push(describe(tokens::COMMON, &all, &expected, &actual));
            }
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {total} cases differ from rg:\n\n{}",
        failures.len(),
        failures.join("\n")
    );
}

#[test]
fn json_matches_ripgrep() {
    if !ripgrep_available() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    mtseek_testkit::generate(dir.path(), &CorpusSpec::default()).unwrap();
    let ways = How::both("json");
    let rg = How::direct();

    let mut failures = Vec::new();
    let mut cases = 0;
    for scope_flags in [&[][..], &["-uu"]] {
        for (pattern, case_flags) in JSON_CASES {
            let mut flags = vec!["--json"];
            flags.extend(case_flags.iter().chain(scope_flags));
            let rg_output = output(
                &rg,
                "rg",
                &["--no-config"],
                &flags,
                pattern,
                dir.path(),
                None,
            );
            let expected = JsonOutcome::new(&rg_output);
            for (how, variant) in ways
                .iter()
                .flat_map(|how| MTSEEK_VARIANTS.iter().map(move |v| (how, v)))
            {
                let exe = env!("CARGO_BIN_EXE_mtseek");
                let actual = JsonOutcome::new(&output(
                    how,
                    exe,
                    variant,
                    &flags,
                    pattern,
                    dir.path(),
                    None,
                ));
                cases += 1;
                let mut all = vec![how.label()];
                all.extend(variant.iter().copied());
                all.extend(&flags);
                if let Some(why) = actual.inconsistent.as_ref() {
                    failures.push(format!("{pattern:?} {all:?}: mtseek's summary: {why}"));
                }
                if expected.code != actual.code
                    || expected.messages != actual.messages
                    || (expected.complete && actual.complete && expected.summary != actual.summary)
                {
                    failures.push(describe_json(pattern, &all, &expected, &actual));
                }
            }
            if let Some(why) = expected.inconsistent {
                failures.push(format!("{pattern:?} {flags:?}: rg's own summary: {why}"));
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

fn run(how: &How, exe: &str, pre: &[&str], flags: &[&str], pattern: &str, root: &Path) -> Outcome {
    run_in(how, exe, pre, flags, pattern, root, None)
}

/// Runs `exe` (in `cwd`, if given) and returns its exit code and output lines.
fn run_in(
    how: &How,
    exe: &str,
    pre: &[&str],
    flags: &[&str],
    pattern: &str,
    root: &Path,
    cwd: Option<&Path>,
) -> Outcome {
    let output = output(how, exe, pre, flags, pattern, root, cwd);
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

fn output(
    how: &How,
    exe: &str,
    pre: &[&str],
    flags: &[&str],
    pattern: &str,
    root: &Path,
    cwd: Option<&Path>,
) -> std::process::Output {
    let mut command = if exe == "rg" {
        Command::new(exe)
    } else {
        how.command(exe)
    };
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }
    command
        .args(pre)
        .args(flags)
        .arg("--")
        .arg(pattern)
        .arg(root)
        .output()
        .unwrap_or_else(|e| panic!("failed to run {exe}: {e}"))
}

/// `--json` output, compared message by message, in any order.
///
/// Timings, `bytes_searched` and the summary's `searches` describe what each tool did, and
/// an index search reads fewer files, so they are set aside (ADR 0015 §3). `bytes_printed`
/// stays: it is equal only if every message is byte for byte ripgrep's. Each tool's summary
/// must add up its own messages; the two summaries are compared when no file of
/// [`UNSTABLE`] is in either output.
#[derive(Debug)]
struct JsonOutcome {
    code: Option<i32>,
    /// Normalized messages (per file), as JSON text -> how many times.
    messages: BTreeMap<String, usize>,
    /// The summary's counts: matched lines, matches, files with a match, bytes printed.
    summary: Option<[u64; 4]>,
    /// Whether the output had no message about an unstable file.
    complete: bool,
    /// Why the summary doesn't add up the messages, if it doesn't.
    inconsistent: Option<String>,
}

impl JsonOutcome {
    fn new(output: &std::process::Output) -> Self {
        let mut messages = BTreeMap::new();
        let mut summary = None;
        let mut complete = true;
        let mut added = [0u64; 4];
        for line in output
            .stdout
            .split(|&b| b == b'\n')
            .filter(|l| !l.is_empty())
        {
            let unstable = UNSTABLE
                .iter()
                .any(|name| String::from_utf8_lossy(line).contains(name));
            let mut message: Value = match serde_json::from_slice(line) {
                Err(_) if unstable => continue,
                Ok(v) => v,
                Err(_) => {
                    // Not JSON (`-c` overriding `--json`): compare the text.
                    *messages
                        .entry(String::from_utf8_lossy(line).into_owned())
                        .or_insert(0) += 1;
                    continue;
                }
            };
            let stats = |m: &Value, key: &str| m["data"]["stats"][key].as_u64().unwrap_or(0);
            match message["type"].as_str() {
                Some("summary") => {
                    summary = Some([
                        stats(&message, "matched_lines"),
                        stats(&message, "matches"),
                        stats(&message, "searches_with_match"),
                        stats(&message, "bytes_printed"),
                    ]);
                    continue;
                }
                Some("end") => {
                    added[0] += stats(&message, "matched_lines");
                    added[1] += stats(&message, "matches");
                    added[2] += 1;
                    added[3] += stats(&message, "bytes_printed");
                    let s = &mut message["data"]["stats"];
                    s["elapsed"] = Value::Null;
                    s["bytes_searched"] = Value::Null;
                }
                _ => {}
            }
            if unstable {
                complete = false;
                continue;
            }
            *messages.entry(message.to_string()).or_insert(0) += 1;
        }
        let inconsistent = match summary {
            Some(s) if s != added => Some(format!("{s:?} but the messages add up to {added:?}")),
            _ => None,
        };
        Self {
            code: output.status.code(),
            messages,
            summary,
            complete,
            inconsistent,
        }
    }
}

fn describe_json(
    pattern: &str,
    flags: &[&str],
    expected: &JsonOutcome,
    actual: &JsonOutcome,
) -> String {
    let show = |l: &str| l.chars().take(200).collect::<String>();
    let mut s = format!(
        "pattern {pattern:?} flags {flags:?}: exit rg={:?} mtseek={:?}, summary rg={:?} mtseek={:?}\n",
        expected.code, actual.code, expected.summary, actual.summary
    );
    for (line, n) in &expected.messages {
        if actual.messages.get(line) != Some(n) {
            _ = writeln!(s, "  only in rg     : {}", show(line));
        }
    }
    for (line, n) in &actual.messages {
        if expected.messages.get(line) != Some(n) {
            _ = writeln!(s, "  only in mtseek : {}", show(line));
        }
    }
    s
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
