//! Invariant 2, index = oracle: for any corpus and query, searching through the index reports
//! exactly what the reference scanner reports. Same files, same lines, same binary warnings.
//! That holds for every filter the index covers and for searches of its subfolders
//! (ADR 0012).

use std::collections::BTreeMap;
use std::fs;
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use mtseek_core::{CaseMode, Filter, PatternKind, Query, Scope};
use mtseek_index::{Index, membership};
use mtseek_walk::{FileHit, Mode, Options};
use proptest::prelude::*;

/// Per matching file: its lines `(number, bytes)` and the binary offset, if any.
type Results = BTreeMap<PathBuf, (Vec<(u64, Vec<u8>)>, Option<u64>)>;

fn collect(
    run: impl FnOnce(
        &(dyn Fn(&FileHit<'_>) -> ControlFlow<()> + Sync),
        &(dyn Fn(&mtseek_core::Error) + Sync),
    ) -> mtseek_core::Result<()>,
) -> mtseek_core::Result<(Results, Vec<String>)> {
    let hits = Mutex::new(Results::new());
    let errors = Mutex::new(Vec::new());
    run(
        &|hit| {
            let lines = hit
                .lines
                .iter()
                .map(|l| (l.line_number, l.bytes.clone()))
                .collect();
            hits.lock()
                .expect("not poisoned")
                .insert(hit.path.to_path_buf(), (lines, hit.binary_offset));
            ControlFlow::Continue(())
        },
        &|err| errors.lock().expect("not poisoned").push(err.to_string()),
    )?;
    let mut errors = errors.into_inner().expect("not poisoned");
    errors.sort();
    Ok((hits.into_inner().expect("not poisoned"), errors))
}

fn by_scan(
    scope: &Scope,
    query: &Query,
    mode: Mode,
) -> mtseek_core::Result<(Results, Vec<String>)> {
    collect(|on_hit, on_error| {
        mtseek_walk::scan(scope, query, mode, Options::default(), on_hit, on_error).map(|_| ())
    })
}

/// Searches `dir` (relative to the index's root `base`) through the index, with `filter`.
fn by_index(
    index: &Index,
    base: &Path,
    dir: &Path,
    filter: Filter,
    query: &Query,
    mode: Mode,
) -> mtseek_core::Result<(Results, Vec<String>)> {
    let root = base.join(dir);
    collect(|on_hit, on_error| {
        let target = mtseek_query::Target {
            root: &root,
            dir,
            filter,
        };
        mtseek_query::search(
            index,
            target,
            query,
            mode,
            Options::default(),
            on_hit,
            on_error,
        )
        .map(|_| ())
    })
}

fn build(scope: &Scope, dir: &Path, name: &str) -> Index {
    let path = dir.join(name);
    let (index, _) = Index::build(scope, Options::default(), &path).expect("index builds");
    assert_eq!(
        index.base().key(),
        mtseek_index::scope_key(scope).expect("scope has a key")
    );
    index
}

/// The search filters an index built with `coverage` answers: every covered combination of
/// ignore files and hidden entries, each with and without binary mode.
fn filters(coverage: Filter) -> Vec<Filter> {
    let covered = membership::covered(&coverage);
    let mut out = Vec::new();
    for bit in [
        membership::DEFAULT,
        membership::NO_IGNORE,
        membership::HIDDEN,
        membership::ALL,
    ] {
        if covered & bit != 0 {
            for binary in [false, true] {
                let mut f = membership::filter(bit);
                f.include_binary = binary;
                out.push(f);
            }
        }
    }
    out
}

/// The folders of the index the walk with `filter` enters, the root first.
fn entered(index: &Index, filter: Filter) -> Vec<PathBuf> {
    let bit = membership::bit(&filter);
    let mut dirs: Vec<PathBuf> = index
        .base()
        .dirs()
        .expect("dirs")
        .into_iter()
        .filter(|e| e.flags & bit != 0)
        .map(|e| e.path)
        .collect();
    dirs.sort();
    dirs
}

/// Compares the scanner on `base/dir` with the index for one query; returns a description of
/// any difference.
fn compare(
    index: &Index,
    base: &Path,
    dir: &Path,
    filter: Filter,
    query: &Query,
    mode: Mode,
) -> Option<String> {
    let scope = Scope::new([base.join(dir)]).with_filter(filter);
    let expected = by_scan(&scope, query, mode);
    let actual = by_index(index, base, dir, filter, query, mode);
    match (expected, actual) {
        (Ok(e), Ok(a)) if e == a => None,
        (Err(_), Err(_)) => None, // an invalid pattern is invalid either way
        (e, a) => {
            let describe = |r: &mtseek_core::Result<(Results, Vec<String>)>| match r {
                Ok((hits, errors)) => {
                    format!("{:#?} errors {errors:?}", hits.keys().collect::<Vec<_>>())
                }
                Err(e) => format!("error {e}"),
            };
            Some(format!(
                "{query:?} {mode:?} in {} filter {filter:?} (index {:?})\n  scan:  {}\n  index: {}",
                dir.display(),
                index.coverage(),
                describe(&e),
                describe(&a)
            ))
        }
    }
}

#[test]
fn the_index_matches_the_scanner_on_the_edge_case_corpus() {
    let corpus = tempfile::tempdir().unwrap();
    let spec = mtseek_testkit::CorpusSpec {
        files: 150,
        ..Default::default()
    };
    mtseek_testkit::generate(corpus.path(), &spec).unwrap();
    let cache = tempfile::tempdir().unwrap();

    let queries: Vec<Query> = [
        (
            mtseek_testkit::tokens::ABSENT,
            PatternKind::Literal,
            CaseMode::Sensitive,
        ),
        (
            mtseek_testkit::tokens::RARE,
            PatternKind::Literal,
            CaseMode::Sensitive,
        ),
        (
            mtseek_testkit::tokens::MEDIUM,
            PatternKind::Literal,
            CaseMode::Sensitive,
        ),
        (
            mtseek_testkit::tokens::COMMON,
            PatternKind::Literal,
            CaseMode::Sensitive,
        ),
        (
            mtseek_testkit::tokens::NEEDLE,
            PatternKind::Literal,
            CaseMode::Sensitive,
        ),
        ("paystack", PatternKind::Literal, CaseMode::Insensitive),
        ("paystack", PatternKind::Literal, CaseMode::Smart),
        ("NEEDLE", PatternKind::Literal, CaseMode::Insensitive),
        ("café", PatternKind::Literal, CaseMode::Insensitive),
        ("straße", PatternKind::Regex, CaseMode::Insensitive),
        (
            r"Connection\w*String",
            PatternKind::Regex,
            CaseMode::Sensitive,
        ),
        (
            r"\b\d{3}-\d{3}-\d{4}\b",
            PatternKind::Regex,
            CaseMode::Sensitive,
        ),
        (
            r"^const x = \w+\.call\(\);$",
            PatternKind::Regex,
            CaseMode::Sensitive,
        ),
        ("$5.00 (", PatternKind::Literal, CaseMode::Sensitive),
        ("needle|import", PatternKind::Regex, CaseMode::Sensitive),
        ("", PatternKind::Regex, CaseMode::Sensitive),
        ("(unclosed", PatternKind::Regex, CaseMode::Sensitive),
    ]
    .into_iter()
    .map(|(pattern, kind, case)| Query {
        pattern: pattern.into(),
        kind,
        case,
    })
    .collect();

    let mut failures = Vec::new();
    // The narrowest and the widest coverage; the widest answers every filter.
    for (name, coverage) in [
        ("default", Filter::unrestricted(0)),
        ("all", Filter::unrestricted(2)),
    ] {
        let scope = Scope::new([corpus.path()]).with_filter(coverage);
        let index = build(&scope, cache.path(), name);
        for filter in filters(coverage) {
            // The root and a few subfolders: each is a search of its own.
            for dir in entered(&index, filter).iter().take(4) {
                for query in &queries {
                    for mode in [Mode::Files, Mode::Lines] {
                        failures.extend(compare(&index, corpus.path(), dir, filter, query, mode));
                    }
                }
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}

/// Pieces of file content: case variants (including non-ASCII ones that fold to ASCII),
/// separators, NULs and line ends.
const PIECES: &[&str] = &[
    "a", "b", "c", "A", "B", "C", "abc", "ABC", "bca", "cab", "é", "É", "k", "K", "\u{212A}", "s",
    "S", "\u{17F}", " ", "\n", "\r\n", "0", "1", "-", "\0",
];

/// One generated file: content, and how it is encoded.
#[derive(Debug, Clone)]
struct GenFile {
    text: String,
    encoding: u8, // 0 plain, 1 UTF-8 BOM, 2 UTF-16LE BOM, 3 UTF-16BE BOM
    hidden: bool,
}

fn gen_file() -> impl Strategy<Value = GenFile> {
    (
        proptest::collection::vec(proptest::sample::select(PIECES), 0..40),
        prop_oneof![6 => Just(0u8), 1 => Just(1u8), 1 => Just(2u8), 1 => Just(3u8)],
        proptest::bool::weighted(0.15),
    )
        .prop_map(|(pieces, encoding, hidden)| GenFile {
            text: pieces.concat(),
            encoding,
            hidden,
        })
}

fn encode(file: &GenFile) -> Vec<u8> {
    match file.encoding {
        1 => [b"\xEF\xBB\xBF".as_slice(), file.text.as_bytes()].concat(),
        2 => [0xFF, 0xFE]
            .into_iter()
            .chain(file.text.encode_utf16().flat_map(u16::to_le_bytes))
            .collect(),
        3 => [0xFE, 0xFF]
            .into_iter()
            .chain(file.text.encode_utf16().flat_map(u16::to_be_bytes))
            .collect(),
        _ => file.text.as_bytes().to_vec(),
    }
}

/// Regex fragments built from pieces that occur in the files.
fn gen_pattern() -> impl Strategy<Value = String> {
    let atom = prop_oneof![
        "[a-cA-C]{1,4}",
        proptest::sample::select(
            &[
                "é", "É", "k", "K", "s", "S", "\u{17F}", ".", r"\w", r"\d", "[ab]", "[^a]", r"\b",
                "^", "$", " ", "0", "-", "[kK]", r"\x00", "[é0]",
            ][..]
        )
        .prop_map(str::to_owned),
    ];
    atom.prop_recursive(3, 16, 4, |inner| {
        prop_oneof![
            proptest::collection::vec(inner.clone(), 1..4).prop_map(|v| v.concat()),
            proptest::collection::vec(inner.clone(), 2..4)
                .prop_map(|v| format!("(?:{})", v.join("|"))),
            inner.clone().prop_map(|s| format!("(?:{s})*")),
            inner.clone().prop_map(|s| format!("(?:{s})+")),
            inner.clone().prop_map(|s| format!("(?:{s})?")),
            inner.prop_map(|s| format!("(?:{s}){{2}}")),
        ]
    })
}

fn gen_query() -> impl Strategy<Value = Query> {
    (
        gen_pattern(),
        prop_oneof![Just(PatternKind::Regex), Just(PatternKind::Literal)],
        prop_oneof![
            Just(CaseMode::Sensitive),
            Just(CaseMode::Insensitive),
            Just(CaseMode::Smart)
        ],
    )
        .prop_map(|(pattern, kind, case)| Query {
            pattern,
            kind,
            case,
        })
}

/// Folders the random corpora put files in; `.git` and `.gitignore` go in the first two.
const FOLDERS: &[&str] = &["", "sub", "sub/deep", ".hid", "node"];
const IGNORES: &[&str] = &["f1\n", "deep/\n", "*\n!f2\n", "node\n", ".hid\n", "!.hid\n"];

proptest! {
    #![proptest_config(ProptestConfig::with_cases(
        std::env::var("PROPTEST_CASES").ok().and_then(|v| v.parse().ok()).unwrap_or(48)
    ))]

    #[test]
    fn the_index_matches_the_scanner_on_random_corpora(
        files in proptest::collection::vec((gen_file(), 0..FOLDERS.len()), 1..12),
        queries in proptest::collection::vec(gen_query(), 1..8),
        ignores in proptest::collection::vec((0..2usize, 0..IGNORES.len()), 0..3),
        repo in any::<bool>(),
        coverage in 0u8..4,
        search in (0u8..4, any::<bool>(), any::<prop::sample::Index>()),
    ) {
        let corpus = tempfile::tempdir().unwrap();
        for (i, (file, folder)) in files.iter().enumerate() {
            let name = if file.hidden { format!(".f{i}") } else { format!("f{i}") };
            let dir = corpus.path().join(FOLDERS[*folder]);
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join(name), encode(file)).unwrap();
        }
        for (folder, ignore) in &ignores {
            let dir = corpus.path().join(FOLDERS[*folder]);
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join(".gitignore"), IGNORES[*ignore]).unwrap();
        }
        if repo {
            fs::create_dir_all(corpus.path().join(".git")).unwrap();
        }
        let cache = tempfile::tempdir().unwrap();
        let coverage = membership::filter(1 << coverage);
        let scope = Scope::new([corpus.path()]).with_filter(coverage);
        let index = build(&scope, cache.path(), "seg");

        // A filter the index covers, and a folder that filter's walk enters.
        let (bit, binary, pick) = search;
        let covered = membership::covered(&coverage);
        let bit = (0..4).map(|b| 1u8 << ((bit + b) % 4)).find(|b| covered & b != 0).unwrap();
        let mut filter = membership::filter(bit);
        filter.include_binary = binary;
        let dirs = entered(&index, filter);
        let dir = pick.get(&dirs);
        for query in &queries {
            for mode in [Mode::Files, Mode::Lines] {
                if let Some(failure) = compare(&index, corpus.path(), dir, filter, query, mode) {
                    prop_assert!(false, "{}", failure);
                }
            }
        }
    }
}
