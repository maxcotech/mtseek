//! The search engine: what answers a search, in the CLI's direct mode and in the daemon
//! (ADR 0013).
//!
//! * Content searches go through the index of the coverage root that covers them, caught up
//!   from the change journal first (ADR 0010, 0011, 0012). Roots no index can answer exactly
//!   are scanned with the reference scanner. See [`content`].
//! * Name searches use the name index of the drive or folder (ADR 0007, 0009). See
//!   [`names`].
//!
//! The CLI makes a [`Engine::direct`] engine for one run. The daemon keeps one
//! [`Engine::resident`] engine: its indexes stay open, it catches them up between searches
//! ([`Engine::refresh`]), and it writes what changed once changes stop ([`Engine::save`]).
//! Both run the same code, so they print the same results.

use std::collections::HashMap;
use std::ops::ControlFlow;
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, Instant};

use mtseek_core::{CaseMode, Error, Filter, Query, Result};
use mtseek_index::Index;
use mtseek_query::Viewer;
use mtseek_walk::{FileHit, Mode, Summary};

pub mod access;
pub mod content;
pub mod names;

pub use access::Caller;

/// Whether a content search uses, builds or skips the index (`--index`, `--reindex`,
/// `--no-index`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum IndexUse {
    /// Use an index that covers the search, if there is one.
    #[default]
    Auto,
    /// Register the roots as coverage roots and build their indexes if needed.
    Build,
    /// Like [`IndexUse::Build`], but rebuild the indexes first.
    Rebuild,
    /// Scan, even where an index covers the search.
    Off,
}

impl IndexUse {
    /// Whether the search asked for the index explicitly.
    pub fn explicit(self) -> bool {
        matches!(self, Self::Build | Self::Rebuild)
    }
}

/// A content search, as the CLI parsed it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchRequest {
    /// The pattern and how to match it.
    pub query: Query,
    /// Files or directories to search, as typed.
    pub roots: Vec<PathBuf>,
    /// Which files under the roots are searched.
    pub filter: Filter,
    /// Paths only, or matching lines.
    pub mode: Mode,
    /// Worker threads (0: ripgrep's default).
    pub threads: usize,
    /// Whether to use the index.
    pub index: IndexUse,
    /// The caller's working directory, when it isn't this process's (the daemon). Relative
    /// roots are found from it, and results are still printed as joined onto the roots as
    /// typed.
    pub cwd: Option<PathBuf>,
}

/// A name search (`--names`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NamesRequest {
    /// Space-separated terms that must all appear in a name.
    pub pattern: String,
    /// How to compare case.
    pub case: CaseMode,
    /// Folders to search under, as typed.
    pub roots: Vec<PathBuf>,
    /// Worker threads (0: all CPUs).
    pub threads: usize,
    /// The caller's working directory, when it isn't this process's.
    pub cwd: Option<PathBuf>,
}

/// What a content search found, besides the hits it reported.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Outcome {
    /// Totals over all roots.
    pub summary: Summary,
    /// Timings and sizes, for `--stats`.
    pub stats: Vec<String>,
}

/// What a name search found.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NamesOutcome {
    /// Matching paths, absolute, in the order to print them.
    pub paths: Vec<PathBuf>,
    /// Timings and sizes, for `--stats`.
    pub stats: Vec<String>,
}

/// Where a search sends what it finds as it goes.
pub trait Report: Sync {
    /// A matching file. [`ControlFlow::Break`] stops the search (the reader went away).
    fn hit(&self, hit: &FileHit<'_>) -> ControlFlow<()>;
    /// A file or folder that couldn't be read. ripgrep prints these and goes on.
    fn error(&self, err: &Error);
    /// A note for a person watching ("building the content index..."). Never results.
    fn note(&self, message: &str);
}

/// One open content index and what the daemon needs to know about it.
#[derive(Debug)]
pub(crate) struct Open {
    pub(crate) index: Index,
    /// When catch-up last changed it.
    pub(crate) changed: Option<Instant>,
}

/// The search engine. See the crate docs.
#[derive(Debug)]
pub struct Engine {
    cache: Option<PathBuf>,
    resident: bool,
    indexes: HashMap<PathBuf, Open>,
    names: names::Names,
}

/// What [`Engine::status`] reports about one content index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexStatus {
    /// The coverage root.
    pub root: PathBuf,
    /// Whether the index honours ignore files.
    pub ignore_files: bool,
    /// Whether the index includes hidden files.
    pub hidden: bool,
    /// Files in the index.
    pub files: u64,
    /// Bytes on disk (base and written delta).
    pub file_bytes: u64,
    /// Whether the delta holds changes not yet written.
    pub unsaved: bool,
    /// Journal positions, one per drive: (letter, next USN).
    pub checkpoints: Vec<(char, i64)>,
}

impl Engine {
    /// An engine for one run of the CLI: indexes are opened when a search needs them and
    /// every change is written before the search runs.
    pub fn direct(cache: Option<PathBuf>) -> Self {
        Self::new(cache, false)
    }

    /// An engine for the daemon: indexes stay open between searches, deltas stay in memory
    /// until [`Engine::save`].
    pub fn resident(cache: Option<PathBuf>) -> Self {
        Self::new(cache, true)
    }

    fn new(cache: Option<PathBuf>, resident: bool) -> Self {
        Self {
            cache,
            resident,
            indexes: HashMap::new(),
            names: names::Names::default(),
        }
    }

    /// The cache directory: `MTSEEK_CACHE_DIR`, else `%LOCALAPPDATA%\mtseek\cache`.
    pub fn default_cache_dir() -> Option<PathBuf> {
        if let Some(dir) = std::env::var_os("MTSEEK_CACHE_DIR") {
            return Some(dir.into());
        }
        let local = std::env::var_os("LOCALAPPDATA")?;
        Some(Path::new(&local).join("mtseek").join("cache"))
    }

    /// The cache directory this engine uses.
    pub fn cache(&self) -> Option<&Path> {
        self.cache.as_deref()
    }

    /// Runs a content search for this process, reporting hits and unreadable files to
    /// `report` as they are found.
    pub fn search(&mut self, request: &SearchRequest, report: &dyn Report) -> Result<Outcome> {
        self.search_as(request, &Caller::process(), report)
    }

    /// Runs a content search for `caller`: it sees what its own `rg` would (ADR 0014).
    pub fn search_as(
        &mut self,
        request: &SearchRequest,
        caller: &dyn Viewer,
        report: &dyn Report,
    ) -> Result<Outcome> {
        let anywhere = content::View {
            viewer: caller,
            disk_roots: None,
            only_indexed: false,
        };
        let Some(cwd) = &request.cwd else {
            return content::search(self, request, report, &anywhere);
        };
        // Another process's working directory: search each root by its absolute path and
        // spell results as joined onto the root as typed. One root at a time, so that every
        // result is spelled from the root that found it.
        let mut outcome = Outcome::default();
        for typed in &request.roots {
            let actual = absolute_from(cwd, typed);
            let one = SearchRequest {
                roots: vec![actual.clone()],
                cwd: None,
                ..request.clone()
            };
            let respelled = Respell {
                inner: report,
                actual: &actual,
                typed,
            };
            let done = content::search(self, &one, &respelled, &anywhere)?;
            outcome.summary.files_searched += done.summary.files_searched;
            outcome.summary.files_matched += done.summary.files_matched;
            outcome.summary.errors += done.summary.errors;
            outcome.stats.extend(done.stats);
        }
        Ok(outcome)
    }

    /// Answers a content search for the service (ADR 0014 §6): `request.roots` as the caller
    /// typed them, `disk_roots` where they are (resolved by the caller, plain drive paths).
    /// Every root must be answered by an index the service holds, exactly, or the search is
    /// [`Error::Declined`] before anything is reported.
    pub fn serve(
        &mut self,
        request: &SearchRequest,
        disk_roots: &[PathBuf],
        caller: &dyn Viewer,
        report: &dyn Report,
    ) -> Result<Outcome> {
        if disk_roots.len() != request.roots.len() {
            return Err(Error::Declined("roots and resolved roots differ".into()));
        }
        let view = content::View {
            viewer: caller,
            disk_roots: Some(disk_roots),
            only_indexed: true,
        };
        content::search(self, request, report, &view)
    }

    /// Runs a name search.
    pub fn names(&mut self, request: &NamesRequest, report: &dyn Report) -> Result<NamesOutcome> {
        names::search(self, request, report)
    }

    /// Catches up every open content index from the change journal (the daemon, between
    /// searches). An index that needs a rebuild is closed; the next search that needs it
    /// rebuilds it. Returns how many indexes changed.
    pub fn refresh(&mut self) -> usize {
        content::refresh(self)
    }

    /// Writes changes kept in memory that are older than `quiet`: deltas of open content
    /// indexes, and name indexes that changed. `Duration::ZERO` writes everything (before
    /// the daemon stops, or after a CLI run). Returns one line per write, for `--stats` or
    /// the daemon's log; failures are lines too, since they only cost the next start time.
    pub fn save(&mut self, quiet: Duration) -> Vec<String> {
        let mut lines = content::save(self, quiet);
        lines.extend(names::save(self, quiet));
        lines
    }

    /// The open content indexes.
    pub fn status(&self) -> Vec<IndexStatus> {
        let mut out: Vec<IndexStatus> = self
            .indexes
            .iter()
            .map(|(root, open)| {
                let coverage = open.index.coverage();
                IndexStatus {
                    root: root.clone(),
                    ignore_files: coverage.respect_ignore_files,
                    hidden: coverage.include_hidden,
                    files: open.index.len(),
                    file_bytes: open.index.file_bytes(),
                    unsaved: open.index.unsaved(),
                    checkpoints: open
                        .index
                        .meta()
                        .checkpoints
                        .iter()
                        .map(|c| (c.letter, c.usn))
                        .collect(),
                }
            })
            .collect();
        out.sort_by(|a, b| a.root.cmp(&b.root));
        out
    }

    /// The name indexes held in memory: (root, entries).
    pub fn names_status(&self) -> Vec<(PathBuf, usize)> {
        self.names.status()
    }
}

/// `typed` found from `cwd`, without `.` components (`C:\work` and `.` give `C:\work`).
/// `..` stays, as the walker would follow it.
pub fn absolute_from(cwd: &Path, typed: &Path) -> PathBuf {
    cwd.join(typed)
        .components()
        .filter(|c| !matches!(c, Component::CurDir))
        .collect()
}

/// Spells paths found under `actual` as joined onto `typed`, as the walker would have
/// joined them had it been given `typed`.
struct Respell<'a> {
    inner: &'a dyn Report,
    actual: &'a Path,
    typed: &'a Path,
}

impl Respell<'_> {
    fn path(&self, path: &Path) -> Option<PathBuf> {
        let rest = path.strip_prefix(self.actual).ok()?;
        // `typed.join("")` would add a separator.
        Some(if rest.as_os_str().is_empty() {
            self.typed.to_path_buf()
        } else {
            self.typed.join(rest)
        })
    }
}

impl Report for Respell<'_> {
    fn hit(&self, hit: &FileHit<'_>) -> ControlFlow<()> {
        let Some(path) = self.path(hit.path) else {
            return self.inner.hit(hit);
        };
        self.inner.hit(&FileHit {
            path: &path,
            lines: hit.lines,
            binary_offset: hit.binary_offset,
        })
    }

    fn error(&self, err: &Error) {
        if let Error::Io { path, source } = err
            && let Some(path) = self.path(path)
        {
            // `io::Error` can't be cloned; its kind and text are what gets printed.
            let source = std::io::Error::new(source.kind(), source.to_string());
            return self.inner.error(&Error::Io { path, source });
        }
        self.inner.error(err);
    }

    fn note(&self, message: &str) {
        self.inner.note(message);
    }
}

pub(crate) fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

pub(crate) fn io_error(path: &Path, source: std::io::Error) -> Error {
    Error::Io {
        path: path.to_path_buf(),
        source,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[derive(Default)]
    struct Collect {
        hits: Mutex<Vec<PathBuf>>,
        errors: Mutex<Vec<String>>,
    }

    impl Report for Collect {
        fn hit(&self, hit: &FileHit<'_>) -> ControlFlow<()> {
            self.hits.lock().unwrap().push(hit.path.to_path_buf());
            ControlFlow::Continue(())
        }
        fn error(&self, err: &Error) {
            self.errors.lock().unwrap().push(err.to_string());
        }
        fn note(&self, _: &str) {}
    }

    #[test]
    fn roots_are_found_from_the_callers_directory() {
        let cwd = Path::new(r"C:\work");
        assert_eq!(absolute_from(cwd, Path::new(".")), Path::new(r"C:\work"));
        assert_eq!(
            absolute_from(cwd, Path::new(r".\src\.\x")),
            Path::new(r"C:\work\src\x")
        );
        assert_eq!(
            absolute_from(cwd, Path::new(r"D:\other")),
            Path::new(r"D:\other")
        );
        assert_eq!(
            absolute_from(cwd, Path::new(r"..\up")),
            Path::new(r"C:\work\..\up")
        );
    }

    #[test]
    fn results_are_spelled_as_joined_onto_the_typed_root() {
        let collect = Collect::default();
        let respell = |typed: &str, path: &str| {
            let actual = absolute_from(Path::new(r"C:\work"), Path::new(typed));
            let r = Respell {
                inner: &collect,
                actual: &actual,
                typed: Path::new(typed),
            };
            let _ = r.hit(&FileHit {
                path: Path::new(path),
                lines: &[],
                binary_offset: None,
            });
            r.error(&Error::Io {
                path: PathBuf::from(path),
                source: std::io::ErrorKind::NotFound.into(),
            });
        };
        respell(".", r"C:\work\a\b.txt");
        respell("./", r"C:\work\a.txt");
        respell("one.txt", r"C:\work\one.txt");
        respell(r"C:\abs", r"C:\abs\x");
        assert_eq!(
            *collect.hits.lock().unwrap(),
            [r".\a\b.txt", "./a.txt", "one.txt", r"C:\abs\x"].map(PathBuf::from)
        );
        let errors = collect.errors.lock().unwrap();
        assert!(errors[0].starts_with(r".\a\b.txt: "), "{errors:?}");
        assert!(errors[2].starts_with("one.txt: "), "{errors:?}");
    }

    #[test]
    fn a_walked_root_and_a_relative_one_give_the_same_spelling() {
        // What the walker prints for root `typed` must equal what respelling prints after
        // searching the absolute root: both join the file's relative path onto the root.
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub").join("f.txt"), "needle\n").unwrap();
        let request = |roots: Vec<PathBuf>, cwd: Option<PathBuf>| SearchRequest {
            query: Query::literal("needle"),
            roots,
            filter: Filter::default(),
            mode: Mode::Files,
            threads: 1,
            index: IndexUse::Off,
            cwd,
        };
        let through_cwd = Collect::default();
        Engine::direct(None)
            .search(
                &request(vec![PathBuf::from("sub")], Some(dir.path().to_path_buf())),
                &through_cwd,
            )
            .unwrap();
        let direct = Collect::default();
        Engine::direct(None)
            .search(&request(vec![dir.path().join("sub")], None), &direct)
            .unwrap();
        let through_cwd = through_cwd.hits.into_inner().unwrap();
        assert_eq!(through_cwd, [Path::new("sub").join("f.txt")]);
        let direct = direct.hits.into_inner().unwrap();
        assert_eq!(direct, [dir.path().join("sub").join("f.txt")]);
    }
}
