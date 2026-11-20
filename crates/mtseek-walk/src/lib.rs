//! The reference scanner: walk a [`Scope`] and search every file in it.
//!
//! This is the slow, obviously-correct path. It has three jobs:
//!
//! 1. **Oracle.** Differential tests compare the index (milestone M2) against it.
//! 2. **Fallback.** It answers queries the index cannot narrow (a regex with no literal
//!    to anchor on) and covers volumes the NTFS engine cannot read.
//! 3. **Freshness.** It searches the "dirty set" of files changed since the last index commit.
//!
//! It is built from the same crates as ripgrep (`ignore`, `grep-searcher`, `grep-regex`),
//! configured the way `rg` configures them. That gives byte-identical results without
//! re-implementing gitignore rules, binary detection or UTF-16 BOM sniffing.
//! `crates/mtseek-cli/tests/parity.rs` checks this against the real `rg` binary.

use std::ops::ControlFlow;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use grep_regex::{RegexMatcher, RegexMatcherBuilder};
use grep_searcher::{BinaryDetection, Searcher, SearcherBuilder, Sink, SinkMatch};
use ignore::{WalkBuilder, WalkState};
use mtseek_core::{CaseMode, Error, PatternKind, Query, Result, Scope};

/// What a scan reports for each matching file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mode {
    /// Report only that the file matches (`rg --files-with-matches`). Stops at the first match.
    #[default]
    Files,
    /// Report every matching line.
    Lines,
}

/// One matching line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LineHit {
    /// 1-based line number.
    pub line_number: u64,
    /// The raw line bytes, including the line terminator when the file has one.
    pub bytes: Vec<u8>,
}

/// One matching file. `lines` is empty in [`Mode::Files`].
#[derive(Debug)]
pub struct FileHit<'a> {
    /// The path, joined onto the scope root exactly as ripgrep would print it.
    pub path: &'a Path,
    /// Matching lines in file order.
    pub lines: &'a [LineHit],
    /// Set when the file turned out to be binary *after* it had matched, so the search stopped
    /// early: the byte offset of the NUL. ripgrep prints a warning line for this in lines mode.
    pub binary_offset: Option<u64>,
}

/// Totals for a finished scan.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Summary {
    /// Files searched.
    pub files_searched: u64,
    /// Files with at least one match.
    pub files_matched: u64,
    /// Files or directories that could not be read.
    pub errors: u64,
}

/// Options that don't change *what* matches, only how the scan runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Options {
    /// Worker threads. `0` picks ripgrep's default (logical CPUs, capped at 12).
    pub threads: usize,
}

/// Compiles a [`Query`] into the matcher ripgrep would build for it.
pub fn build_matcher(query: &Query) -> Result<RegexMatcher> {
    let mut builder = RegexMatcherBuilder::new();
    builder.line_terminator(Some(b'\n'));
    match query.case {
        CaseMode::Sensitive => {}
        CaseMode::Insensitive => {
            builder.case_insensitive(true);
        }
        CaseMode::Smart => {
            builder.case_smart(true);
        }
    }
    let pattern = match query.kind {
        PatternKind::Regex => query.pattern.clone(),
        PatternKind::Literal => regex_syntax::escape(&query.pattern),
    };
    builder
        .build(&pattern)
        .map_err(|e| Error::InvalidPattern(e.to_string()))
}

/// Walks `scope` and searches every file for `query`.
///
/// `on_hit` runs once per matching file, from worker threads, in no particular order.
/// Return [`ControlFlow::Break`] to stop the scan early (for example when stdout closes).
/// `on_error` runs for every unreadable file or directory; the scan carries on.
pub fn scan(
    scope: &Scope,
    query: &Query,
    mode: Mode,
    options: Options,
    on_hit: &(dyn Fn(&FileHit<'_>) -> ControlFlow<()> + Sync),
    on_error: &(dyn Fn(&Error) + Sync),
) -> Result<Summary> {
    let matcher = build_matcher(query)?;
    let counts = Counts::default();

    let report_error = |err: Error| {
        counts.errors.fetch_add(1, Ordering::Relaxed);
        on_error(&err);
    };

    walker(scope, options).run(|| {
        let matcher = matcher.clone();
        let mut searcher = searcher(scope);
        let mut lines = Vec::new();
        let report_error = &report_error;
        let counts = &counts;
        Box::new(move |entry| {
            let entry = match entry {
                Ok(entry) => entry,
                Err(err) => {
                    report_error(walk_error(err));
                    return WalkState::Continue;
                }
            };
            if !entry.file_type().is_some_and(|t| t.is_file()) {
                return WalkState::Continue;
            }
            counts.searched.fetch_add(1, Ordering::Relaxed);
            lines.clear();
            let mut sink = Collect {
                mode,
                lines: &mut lines,
                matched: false,
                binary_offset: None,
            };
            if let Err(source) = searcher.search_path(&matcher, entry.path(), &mut sink) {
                report_error(Error::Io {
                    path: entry.path().to_path_buf(),
                    source,
                });
                return WalkState::Continue;
            }
            if !sink.matched {
                return WalkState::Continue;
            }
            counts.matched.fetch_add(1, Ordering::Relaxed);
            let binary_offset = sink.binary_offset;
            match on_hit(&FileHit {
                path: entry.path(),
                lines: &lines,
                binary_offset,
            }) {
                ControlFlow::Continue(()) => WalkState::Continue,
                ControlFlow::Break(()) => WalkState::Quit,
            }
        })
    });

    Ok(Summary {
        files_searched: counts.searched.into_inner(),
        files_matched: counts.matched.into_inner(),
        errors: counts.errors.into_inner(),
    })
}

#[derive(Default)]
struct Counts {
    searched: AtomicU64,
    matched: AtomicU64,
    errors: AtomicU64,
}

/// ripgrep's default thread count: logical CPUs, capped at 12.
fn default_threads() -> usize {
    std::thread::available_parallelism()
        .map_or(1, std::num::NonZero::get)
        .min(12)
}

fn walker(scope: &Scope, options: Options) -> ignore::WalkParallel {
    let mut roots = scope.roots.iter();
    let mut builder =
        WalkBuilder::new(roots.next().map_or_else(|| Path::new("."), |p| p.as_path()));
    for root in roots {
        builder.add(root);
    }
    let filter = scope.filter;
    builder
        .standard_filters(filter.respect_ignore_files)
        .hidden(!filter.include_hidden)
        .follow_links(filter.follow_links)
        .threads(if options.threads == 0 {
            default_threads()
        } else {
            options.threads
        });
    if filter.respect_ignore_files {
        builder.add_custom_ignore_filename(".rgignore");
    }
    builder.build_parallel()
}

fn searcher(scope: &Scope) -> Searcher {
    // Files found by walking a directory get ripgrep's "implicit" binary handling:
    // stop at the first NUL byte. `--binary` (-uuu) searches them, converting NULs.
    let binary = if scope.filter.include_binary {
        BinaryDetection::convert(b'\x00')
    } else {
        BinaryDetection::quit(b'\x00')
    };
    SearcherBuilder::new()
        .line_number(true)
        .binary_detection(binary)
        .build()
}

fn walk_error(err: ignore::Error) -> Error {
    let path = walk_error_path(&err).unwrap_or_default();
    let source = err
        .into_io_error()
        .unwrap_or_else(|| std::io::Error::other("directory walk failed"));
    Error::Io { path, source }
}

fn walk_error_path(err: &ignore::Error) -> Option<std::path::PathBuf> {
    match err {
        ignore::Error::WithPath { path, .. } => Some(path.clone()),
        ignore::Error::WithDepth { err, .. } | ignore::Error::WithLineNumber { err, .. } => {
            walk_error_path(err)
        }
        _ => None,
    }
}

struct Collect<'a> {
    mode: Mode,
    lines: &'a mut Vec<LineHit>,
    matched: bool,
    binary_offset: Option<u64>,
}

impl Sink for Collect<'_> {
    type Error = std::io::Error;

    fn matched(&mut self, _: &Searcher, m: &SinkMatch<'_>) -> Result<bool, Self::Error> {
        self.matched = true;
        if self.mode == Mode::Files {
            return Ok(false); // one match is enough
        }
        self.lines.push(LineHit {
            line_number: m.line_number().unwrap_or(0),
            bytes: m.bytes().to_vec(),
        });
        Ok(true)
    }

    fn binary_data(&mut self, searcher: &Searcher, offset: u64) -> Result<bool, Self::Error> {
        // Only "quit" mode cuts a search short; "convert" mode (-uuu) keeps going.
        if searcher.binary_detection().quit_byte().is_some() {
            self.binary_offset = Some(offset);
        }
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::Mutex;

    fn files(scope: &Scope, query: &Query) -> BTreeSet<PathBuf> {
        let out = Mutex::new(BTreeSet::new());
        scan(
            scope,
            query,
            Mode::Files,
            Options::default(),
            &|hit| {
                out.lock().unwrap().insert(
                    hit.path
                        .strip_prefix(&scope.roots[0])
                        .unwrap()
                        .to_path_buf(),
                );
                ControlFlow::Continue(())
            },
            &|err| panic!("{err}"),
        )
        .unwrap();
        out.into_inner().unwrap()
    }

    fn set(paths: &[&str]) -> BTreeSet<PathBuf> {
        paths.iter().map(PathBuf::from).collect()
    }

    #[test]
    fn respects_gitignore_only_inside_a_git_repo() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("repo/.git")).unwrap();
        fs::write(root.join("repo/.gitignore"), "ignored.txt\n").unwrap();
        fs::write(root.join("repo/ignored.txt"), "needle\n").unwrap();
        fs::write(root.join("repo/kept.txt"), "needle\n").unwrap();
        fs::create_dir_all(root.join("plain")).unwrap();
        fs::write(root.join("plain/.gitignore"), "ignored.txt\n").unwrap();
        fs::write(root.join("plain/ignored.txt"), "needle\n").unwrap();

        let scope = Scope::new([root]);
        // `plain/` is not a git repo, so its .gitignore is not honoured (ripgrep's require_git).
        let expected = set(&["plain/ignored.txt", "repo/kept.txt"]);
        assert_eq!(files(&scope, &Query::literal("needle")), expected);

        let scope = scope.with_filter(mtseek_core::Filter::unrestricted(1));
        assert_eq!(files(&scope, &Query::literal("needle")).len(), 3);
    }

    #[test]
    fn skips_hidden_and_binary_by_default() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join(".hidden.txt"), "needle\n").unwrap();
        fs::write(root.join("binary.bin"), b"\x00needle\n").unwrap();
        fs::write(root.join("text.txt"), "needle\n").unwrap();

        let q = Query::literal("needle");
        assert_eq!(files(&Scope::new([root]), &q), set(&["text.txt"]));
        let uu = Scope::new([root]).with_filter(mtseek_core::Filter::unrestricted(2));
        assert_eq!(files(&uu, &q), set(&[".hidden.txt", "text.txt"]));
        let uuu = Scope::new([root]).with_filter(mtseek_core::Filter::unrestricted(3));
        assert_eq!(
            files(&uuu, &q),
            set(&[".hidden.txt", "binary.bin", "text.txt"])
        );
    }

    #[test]
    fn finds_text_in_utf16_files() {
        let dir = tempfile::tempdir().unwrap();
        let mut bytes = vec![0xFF, 0xFE]; // UTF-16LE BOM
        bytes.extend("hello needle\n".encode_utf16().flat_map(u16::to_le_bytes));
        fs::write(dir.path().join("utf16.txt"), bytes).unwrap();
        assert_eq!(
            files(&Scope::new([dir.path()]), &Query::literal("needle")),
            set(&["utf16.txt"])
        );
    }

    #[test]
    fn binary_after_match_reports_lines_and_offset() {
        let dir = tempfile::tempdir().unwrap();
        let mut bytes = b"needle first\n".to_vec();
        bytes.extend(std::iter::repeat_n(b"short line\n".as_slice(), 20_000).flatten());
        let nul_at = bytes.len() as u64;
        bytes.extend_from_slice(b"\0 needle after\n");
        fs::write(dir.path().join("late.bin"), &bytes).unwrap();

        let seen = Mutex::new(Vec::new());
        scan(
            &Scope::new([dir.path()]),
            &Query::literal("needle"),
            Mode::Lines,
            Options::default(),
            &|hit| {
                let numbers: Vec<u64> = hit.lines.iter().map(|l| l.line_number).collect();
                seen.lock().unwrap().push((numbers, hit.binary_offset));
                ControlFlow::Continue(())
            },
            &|err| panic!("{err}"),
        )
        .unwrap();
        assert_eq!(seen.into_inner().unwrap(), vec![(vec![1], Some(nul_at))]);
    }

    #[test]
    fn literal_mode_escapes_regex_metacharacters() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.txt"), "cost is $5.00 (approx)\n").unwrap();
        fs::write(dir.path().join("b.txt"), "cost is $5X00 approx\n").unwrap();
        let q = Query::literal("$5.00 (approx)");
        assert_eq!(files(&Scope::new([dir.path()]), &q), set(&["a.txt"]));
    }

    #[test]
    fn case_modes() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("upper.txt"), "Needle\n").unwrap();
        fs::write(dir.path().join("lower.txt"), "needle\n").unwrap();
        let scope = Scope::new([dir.path()]);
        assert_eq!(
            files(&scope, &Query::literal("needle")),
            set(&["lower.txt"])
        );
        let both = set(&["lower.txt", "upper.txt"]);
        assert_eq!(
            files(
                &scope,
                &Query::literal("needle").with_case(CaseMode::Insensitive)
            ),
            both
        );
        assert_eq!(
            files(&scope, &Query::literal("needle").with_case(CaseMode::Smart)),
            both
        );
        let smart_upper = Query::literal("Needle").with_case(CaseMode::Smart);
        assert_eq!(files(&scope, &smart_upper), set(&["upper.txt"]));
    }

    #[test]
    fn lines_mode_reports_every_matching_line() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.txt"), "one needle\ntwo\nthree needle").unwrap();
        let lines = Mutex::new(Vec::new());
        scan(
            &Scope::new([dir.path()]),
            &Query::literal("needle"),
            Mode::Lines,
            Options::default(),
            &|hit| {
                lines.lock().unwrap().extend(hit.lines.iter().cloned());
                ControlFlow::Continue(())
            },
            &|err| panic!("{err}"),
        )
        .unwrap();
        let lines = lines.into_inner().unwrap();
        assert_eq!(
            lines,
            vec![
                LineHit {
                    line_number: 1,
                    bytes: b"one needle\n".to_vec()
                },
                LineHit {
                    line_number: 3,
                    bytes: b"three needle".to_vec()
                },
            ]
        );
    }

    #[test]
    fn invalid_regex_is_an_error_not_a_panic() {
        let err = build_matcher(&Query::regex("(unclosed")).unwrap_err();
        assert!(matches!(err, Error::InvalidPattern(_)));
    }
}
