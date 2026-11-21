//! Invariant 2, index = oracle: for any corpus and query, searching through the index reports
//! exactly what the reference scanner reports. Same files, same lines, same binary warnings.

use std::collections::BTreeMap;
use std::fs;
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use mtseek_core::{CaseMode, Filter, PatternKind, Query, Scope};
use mtseek_index::Segment;
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

fn by_index(
    segment: &Segment,
    scope: &Scope,
    query: &Query,
    mode: Mode,
) -> mtseek_core::Result<(Results, Vec<String>)> {
    collect(|on_hit, on_error| {
        mtseek_query::search(
            segment,
            scope,
            query,
            mode,
            Options::default(),
            on_hit,
            on_error,
        )
        .map(|_| ())
    })
}

fn build(scope: &Scope, dir: &Path, name: &str) -> Segment {
    let path = dir.join(name);
    mtseek_index::build(scope, Options::default(), &path).expect("index builds");
    let segment = Segment::open(&path).expect("index opens");
    assert_eq!(
        segment.key(),
        mtseek_index::scope_key(scope).expect("scope has a key")
    );
    segment
}

/// Compares both paths for one query; returns a description of any difference.
fn compare(segment: &Segment, scope: &Scope, query: &Query, mode: Mode) -> Option<String> {
    let expected = by_scan(scope, query, mode);
    let actual = by_index(segment, scope, query, mode);
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
                "{query:?} {mode:?} filter {:?}\n  scan:  {}\n  index: {}",
                scope.filter,
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
    for level in 0..=3 {
        let scope = Scope::new([corpus.path()]).with_filter(Filter::unrestricted(level));
        let segment = build(&scope, cache.path(), &format!("u{level}"));
        for query in &queries {
            for mode in [Mode::Files, Mode::Lines] {
                failures.extend(compare(&segment, &scope, query, mode));
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

proptest! {
    #![proptest_config(ProptestConfig::with_cases(48))]

    #[test]
    fn the_index_matches_the_scanner_on_random_corpora(
        files in proptest::collection::vec(gen_file(), 1..12),
        queries in proptest::collection::vec(gen_query(), 1..8),
        level in 0u8..=3,
    ) {
        let corpus = tempfile::tempdir().unwrap();
        for (i, file) in files.iter().enumerate() {
            let name = if file.hidden { format!(".f{i}") } else { format!("f{i}") };
            let dir = if i % 3 == 0 { corpus.path().join("sub") } else { corpus.path().to_path_buf() };
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join(name), encode(file)).unwrap();
        }
        let cache = tempfile::tempdir().unwrap();
        let scope = Scope::new([corpus.path()]).with_filter(Filter::unrestricted(level));
        let segment = build(&scope, cache.path(), "seg");
        for query in &queries {
            for mode in [Mode::Files, Mode::Lines] {
                if let Some(failure) = compare(&segment, &scope, query, mode) {
                    prop_assert!(false, "{}", failure);
                }
            }
        }
    }
}
