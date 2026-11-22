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
//!
//! The content index (`mtseek-index`, `mtseek-query`) reuses three pieces: [`list`] for
//! the file set of a scope, [`FileSearcher`] to verify candidates, and [`Rules`] for globs
//! and file types, so both paths run the same code (ADR 0010, 0015).

use std::io;
use std::ops::ControlFlow;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use grep_matcher::Matcher;
pub use grep_regex::RegexMatcher;
use grep_regex::RegexMatcherBuilder;
use grep_searcher::{
    BinaryDetection, Searcher, SearcherBuilder, Sink, SinkContext, SinkFinish, SinkMatch,
};
use ignore::overrides::{Override, OverrideBuilder};
use ignore::types::{Types, TypesBuilder};
use ignore::{WalkBuilder, WalkState};
use mtseek_core::{
    CaseMode, Error, Filter, IgnoreFiles, PatternKind, Query, Result, Scope, Select,
};

/// What a search reports for each matching file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mode {
    /// Report only that the file matches (`rg --files-with-matches`). Stops at the first match.
    #[default]
    Files,
    /// Count matching lines (`rg --count`).
    Count,
    /// Count matches (`rg --count-matches`).
    CountMatches,
    /// Report every matching line, and context lines if asked for.
    Lines,
}

/// What a search reports and how it finds lines: ripgrep's output mode, `-A`, `-B` and `-m`.
///
/// `grep-searcher` applies all of it, configured as `rg` configures it, so the scanner and
/// the index's verifier report the same lines (ADR 0015).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Output {
    /// What is reported per file.
    pub mode: Mode,
    /// Lines of context before each match (`-B`).
    pub before: usize,
    /// Lines of context after each match (`-A`).
    pub after: usize,
    /// Stop searching a file after this many matching lines (`-m`). As in ripgrep, the
    /// context after the last one is still reported, and a match inside it counts.
    pub max_count: Option<u64>,
    /// In [`Mode::Lines`], find where each match is in its line (`rg --json`).
    pub positions: bool,
}

impl From<Mode> for Output {
    fn from(mode: Mode) -> Self {
        Self {
            mode,
            ..Self::default()
        }
    }
}

impl Output {
    /// Whether context lines are asked for.
    pub fn context(&self) -> bool {
        self.before > 0 || self.after > 0
    }
}

/// One line reported for a file: a matching line, or a context line around one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LineHit {
    /// 1-based line number.
    pub line_number: u64,
    /// Where the line starts in the searched text, in bytes. For a UTF-16 file that is the
    /// decoded text, as in ripgrep.
    pub offset: u64,
    /// The raw line bytes, including the line terminator when the file has one.
    pub bytes: Vec<u8>,
    /// A context line (`-A`, `-B`), not a matching one.
    pub context: bool,
    /// Where each match is in `bytes`, as (start, end). Filled only with
    /// [`Output::positions`], and never for context lines.
    pub matches: Vec<(usize, usize)>,
}

impl LineHit {
    /// A matching line without match positions.
    pub fn matched(line_number: u64, offset: u64, bytes: Vec<u8>) -> Self {
        Self {
            line_number,
            offset,
            bytes,
            context: false,
            matches: Vec::new(),
        }
    }
}

/// What the search of one file found and cost.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FileStats {
    /// Matching lines (1 in [`Mode::Files`], which stops at the first).
    pub matched_lines: u64,
    /// Matches, counted in [`Mode::CountMatches`] and with [`Output::positions`]; else 0.
    pub matches: u64,
    /// Bytes the searcher read.
    pub bytes_searched: u64,
    /// How long the search took.
    pub elapsed: Duration,
}

/// One matching file. `lines` is empty except in [`Mode::Lines`].
#[derive(Debug)]
pub struct FileHit<'a> {
    /// The path, joined onto the scope root exactly as ripgrep would print it.
    pub path: &'a Path,
    /// Matching lines and context lines, in file order.
    pub lines: &'a [LineHit],
    /// Set when the file turned out to be binary *after* it had matched, so the search stopped
    /// early: the byte offset of the NUL. ripgrep prints a warning line for this in lines mode.
    pub binary_offset: Option<u64>,
    /// Counts and cost.
    pub stats: FileStats,
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
    /// Bytes searched, over all files.
    pub bytes_searched: u64,
    /// Time spent searching files, added up over all of them.
    pub search_time: Duration,
}

impl Summary {
    /// Adds `other`'s totals to these.
    pub fn add(&mut self, other: Self) {
        self.files_searched += other.files_searched;
        self.files_matched += other.files_matched;
        self.errors += other.errors;
        self.bytes_searched += other.bytes_searched;
        self.search_time += other.search_time;
    }

    /// Adds one searched file's bytes and time (not whether it matched).
    pub fn add_file(&mut self, stats: &FileStats) {
        self.files_searched += 1;
        self.bytes_searched += stats.bytes_searched;
        self.search_time += stats.elapsed;
    }
}

/// Options that don't change *what* matches, only how the scan runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Options {
    /// Worker threads. `0` picks ripgrep's default (logical CPUs, capped at 12).
    pub threads: usize,
}

impl Options {
    /// The number of worker threads to use: [`Options::threads`], or ripgrep's default.
    pub fn thread_count(self) -> usize {
        if self.threads == 0 {
            default_threads()
        } else {
            self.threads
        }
    }
}

/// Searches one file's bytes exactly as ripgrep does when it walks a directory.
///
/// The scanner feeds it files; the content index feeds it the stored bytes of candidates.
/// With memory maps off (as here, and as `rg` does for directory walks), `grep-searcher`'s
/// `search_path` is just `File::open` followed by `search_reader`, so [`search_bytes`]
/// over a file's bytes behaves exactly like [`search_path`] over the file.
///
/// Every file is searched as if by a fresh searcher, which is what `rg` reports for a file
/// searched alone. `grep-searcher`'s line buffer grows to fit a long line and is never
/// shrunk, and in quit-on-NUL mode the searcher stops before searching a buffer fill that
/// contains the NUL. Reusing a grown buffer can therefore hide a match that comes shortly
/// before a NUL: `rg` reuses one searcher per thread and gives scheduling-dependent results
/// for such files. So after any file big enough to have grown the buffer, the searcher is
/// rebuilt.
///
/// [`search_bytes`]: FileSearcher::search_bytes
/// [`search_path`]: FileSearcher::search_path
pub struct FileSearcher {
    searcher: Searcher,
    filter: Filter,
    output: Output,
    lines: Vec<LineHit>,
    /// What the last search found and cost.
    last: FileStats,
    /// The last file may have grown the line buffer.
    stale: bool,
}

/// Files this big may grow the line buffer (64 KiB of decoded text; UTF-16 can decode to
/// 1.5 times its size).
const MAY_GROW_BUFFER: u64 = 32 * 1024;

/// A file that matched, as reported by [`FileSearcher`].
#[derive(Debug, Clone, Copy)]
pub struct Found<'a> {
    /// Matching and context lines in file order. Empty except in [`Mode::Lines`].
    pub lines: &'a [LineHit],
    /// See [`FileHit::binary_offset`].
    pub binary_offset: Option<u64>,
    /// Counts and cost.
    pub stats: FileStats,
}

impl FileSearcher {
    /// A searcher configured like ripgrep for `filter` (binary handling) and `output`.
    pub fn new(filter: Filter, output: impl Into<Output>) -> Self {
        let output = output.into();
        Self {
            searcher: searcher(filter, output),
            filter,
            output,
            lines: Vec::new(),
            last: FileStats::default(),
            stale: false,
        }
    }

    /// Searches the file at `path`. `None` if nothing matched.
    pub fn search_path(
        &mut self,
        matcher: &RegexMatcher,
        path: &Path,
    ) -> io::Result<Option<Found<'_>>> {
        let started = Instant::now();
        self.last = FileStats::default();
        // `grep_searcher::Searcher::search_path` is this open followed by `search_file`.
        let file = std::fs::File::open(path)?;
        let len = file.metadata().map_or(u64::MAX, |m| m.len());
        self.prepare(len);
        let mut sink = Collect::new(self.output, matcher, &mut self.lines);
        self.searcher.search_file(matcher, &file, &mut sink)?;
        let done = sink.done();
        Ok(self.found(done, started))
    }

    /// Searches `bytes` as if they were a file's contents. `None` if nothing matched.
    pub fn search_bytes(
        &mut self,
        matcher: &RegexMatcher,
        bytes: &[u8],
    ) -> io::Result<Option<Found<'_>>> {
        let started = Instant::now();
        self.last = FileStats::default();
        self.prepare(bytes.len() as u64);
        let mut sink = Collect::new(self.output, matcher, &mut self.lines);
        self.searcher.search_reader(matcher, bytes, &mut sink)?;
        let done = sink.done();
        Ok(self.found(done, started))
    }

    /// What the last search found and cost, whether or not it matched.
    pub fn last(&self) -> FileStats {
        self.last
    }

    /// Gets ready to search a file of `len` bytes: a fresh searcher if the last file may
    /// have grown the line buffer.
    fn prepare(&mut self, len: u64) {
        if self.stale {
            self.searcher = searcher(self.filter, self.output);
        }
        self.stale = len >= MAY_GROW_BUFFER;
        self.lines.clear();
    }

    fn found(&mut self, done: Done, started: Instant) -> Option<Found<'_>> {
        self.last = FileStats {
            elapsed: started.elapsed(),
            ..done.stats
        };
        done.matched.then_some(Found {
            lines: &self.lines,
            binary_offset: done.binary_offset,
            stats: self.last,
        })
    }
}

/// Glob and file-type rules ([`Select`]), compiled, applied the way ripgrep applies them.
///
/// ripgrep checks them before ignore files and hidden names, and they can list files those
/// would leave out:
///
/// * A path that matches a glob is searched (or entered, for a folder) even if it is ignored
///   or hidden. With any glob that doesn't start with `!`, a file that matches none is left
///   out. A folder that matches a `!` glob is not entered.
/// * Otherwise ignore files apply. Then a file of a `-T` type is left out, and a file of a
///   `-t` type is searched even if it is hidden. With any `-t`, other files are left out.
/// * Globs are matched against the path relative to the working directory when it is
///   inside it, else against the whole path. Types look at the file name only.
///
/// They also carry which ignore files the walk reads, when that isn't all or none
/// ([`IgnoreFiles`]: `--no-ignore-vcs`, `--ignore-file` and the like).
#[derive(Debug, Clone, Default)]
pub struct Rules {
    globs: Option<Override>,
    types: Option<Types>,
    selects_types: bool,
    ignore_files: IgnoreFiles,
}

/// What a rule says about a path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Searched (a file) or entered (a folder), whatever else applies.
    Include,
    /// Left out.
    Exclude,
    /// The rule doesn't decide; the next one does.
    Pass,
}

/// Which files beyond its filter's a walk with [`Rules`] may list.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Widening {
    /// None: the rules only leave files out.
    None,
    /// Hidden files in folders the walk enters (a `-t` type).
    Hidden,
    /// Hidden and ignored files in folders the walk enters, and folders it wouldn't enter
    /// (a glob without `!`).
    All,
}

impl Rules {
    /// Compiles `select`. Globs are matched relative to `cwd`, as ripgrep matches them
    /// relative to its working directory.
    pub fn new(select: &Select, cwd: &Path) -> Result<Self> {
        let invalid = |e: ignore::Error| Error::InvalidArgument(e.to_string());
        let globs = if select.globs.is_empty() && select.iglobs.is_empty() {
            None
        } else {
            let mut builder = OverrideBuilder::new(cwd);
            for glob in &select.globs {
                builder.add(glob).map_err(invalid)?;
            }
            builder.case_insensitive(true).map_err(invalid)?;
            for glob in &select.iglobs {
                builder.add(glob).map_err(invalid)?;
            }
            Some(builder.build().map_err(invalid)?)
        };
        let types = if select.types.is_empty() {
            None
        } else {
            let mut builder = TypesBuilder::new();
            builder.add_defaults();
            for rule in &select.types {
                if rule.negated {
                    builder.negate(&rule.name);
                } else {
                    builder.select(&rule.name);
                }
            }
            Some(builder.build().map_err(invalid)?)
        };
        Ok(Self {
            globs,
            types,
            selects_types: select.types.iter().any(|t| !t.negated),
            ignore_files: select.ignore_files.clone(),
        })
    }

    /// Whether there are no rules.
    pub fn is_empty(&self) -> bool {
        self.globs.is_none() && self.types.is_none() && self.ignore_files == IgnoreFiles::default()
    }

    /// Which ignore files the walk reads.
    pub fn ignore_files(&self) -> &IgnoreFiles {
        &self.ignore_files
    }

    /// Which files beyond its filter's a walk with these rules may list.
    pub fn widening(&self) -> Widening {
        if self.globs.as_ref().is_some_and(|g| g.num_whitelists() > 0) {
            Widening::All
        } else if self.selects_types {
            Widening::Hidden
        } else {
            Widening::None
        }
    }

    /// Whether a `!` glob can leave out folders the walk would otherwise enter.
    pub fn prunes(&self) -> bool {
        self.globs.as_ref().is_some_and(|g| g.num_ignores() > 0)
    }

    /// What the globs say about `path`, spelled as the walker spells it (joined onto the
    /// root as typed).
    pub fn glob(&self, path: &Path, is_dir: bool) -> Verdict {
        let Some(globs) = &self.globs else {
            return Verdict::Pass;
        };
        // As `ignore`'s walker does before matching.
        let path = path.strip_prefix("./").unwrap_or(path);
        verdict(&globs.matched(path, is_dir))
    }

    /// What the file types say about the file at `path`.
    pub fn file_type(&self, path: &Path) -> Verdict {
        match &self.types {
            Some(types) => verdict(&types.matched(path, false)),
            None => Verdict::Pass,
        }
    }

    /// Whether a walk with these rules lists the file at `path`, from what is known about it
    /// without them. `not_ignored` says whether ignore files leave it out (it may be
    /// hidden); it is asked only for a file of a `-t` type the walk alone doesn't list. The
    /// folders above it are the caller's to check ([`Rules::glob`]).
    pub fn lists(&self, path: &Path, known: Known, not_ignored: &mut dyn FnMut() -> bool) -> bool {
        match self.glob(path, false) {
            Verdict::Include => known.exists,
            Verdict::Exclude => false,
            Verdict::Pass => match self.file_type(path) {
                Verdict::Include => known.listed || (known.exists && not_ignored()),
                Verdict::Exclude => false,
                Verdict::Pass => known.listed,
            },
        }
    }
}

/// What is known about a file, for [`Rules::lists`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Known {
    /// The walk without rules lists it.
    pub listed: bool,
    /// It is there, in a folder the walk enters.
    pub exists: bool,
}

fn verdict<T>(m: &ignore::Match<T>) -> Verdict {
    match m {
        ignore::Match::Whitelist(_) => Verdict::Include,
        ignore::Match::Ignore(_) => Verdict::Exclude,
        ignore::Match::None => Verdict::Pass,
    }
}

/// One file or directory found by [`list`].
#[derive(Debug, Clone, Copy)]
pub struct Listed<'a> {
    /// The index of the root it was found under.
    pub root: usize,
    /// Its path, joined onto that root.
    pub path: &'a Path,
    /// A directory the walk enters (or would enter, below `max_depth`). Otherwise a file.
    pub is_dir: bool,
    /// 0 for the root itself.
    pub depth: usize,
}

/// Lists what `scope` covers: the files `rg --files` lists with the same flags, and the
/// directories the walk enters to find them (the roots included).
///
/// `max_depth` stops the walk below that depth (`Some(1)`: a root's direct entries).
/// `on_entry` runs for every file and directory, from worker threads when
/// `options` asks for more than one. `on_error` runs for every unreadable directory.
pub fn list(
    scope: &Scope,
    options: Options,
    max_depth: Option<usize>,
    on_entry: &(dyn Fn(Listed<'_>) + Sync),
    on_error: &(dyn Fn(&Error) + Sync),
) {
    let visit =
        |root: usize, entry: std::result::Result<ignore::DirEntry, ignore::Error>| match entry {
            Ok(entry) => {
                let Some(file_type) = entry.file_type() else {
                    return;
                };
                if file_type.is_file() || file_type.is_dir() {
                    on_entry(Listed {
                        root,
                        path: entry.path(),
                        is_dir: file_type.is_dir(),
                        depth: entry.depth(),
                    });
                }
            }
            Err(err) => on_error(&walk_error(err)),
        };
    for (root_index, root) in scope.roots.iter().enumerate() {
        let (mut builder, _) = walk_builder(
            std::slice::from_ref(root),
            scope.filter,
            &Rules::default(),
            options,
        );
        builder.max_depth(max_depth);
        if options.thread_count() == 1 {
            for entry in builder.build() {
                visit(root_index, entry);
            }
        } else {
            builder.build_parallel().run(|| {
                Box::new(move |entry| {
                    visit(root_index, entry);
                    WalkState::Continue
                })
            });
        }
    }
}

/// Git's global excludes file as ripgrep finds it for this process (from git's
/// configuration and the environment), and its contents (empty if it doesn't exist).
/// `None` if there is no such file to look for.
pub fn global_excludes() -> Option<(std::path::PathBuf, Vec<u8>)> {
    let path = ignore::gitignore::gitconfig_excludes_path()?;
    let bytes = std::fs::read(&path).unwrap_or_default();
    Some((path, bytes))
}

/// Whether an ignore file's contents hold any rule: a line that is neither blank nor a
/// comment.
pub fn has_ignore_rules(bytes: &[u8]) -> bool {
    bytes.split(|&b| b == b'\n').any(|line| {
        let line = line.trim_ascii();
        !line.is_empty() && !line.starts_with(b"#")
    })
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

/// Walks `scope`, narrowed or widened by `rules`, and searches every file for `query`.
///
/// `on_hit` runs once per matching file, from worker threads, in no particular order.
/// Return [`ControlFlow::Break`] to stop the scan early (for example when stdout closes).
/// `on_error` runs for every unreadable file or directory; the scan carries on.
pub fn scan(
    scope: &Scope,
    rules: &Rules,
    query: &Query,
    output: Output,
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

    let (builder, unreadable) = walk_builder(&scope.roots, scope.filter, rules, options);
    // ripgrep reports an ignore file it can't read and goes on; it doesn't count as an error.
    for err in unreadable {
        on_error(&Error::InvalidArgument(err.to_string()));
    }
    builder.build_parallel().run(|| {
        let matcher = matcher.clone();
        let mut searcher = FileSearcher::new(scope.filter, output);
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
            let found = match searcher.search_path(&matcher, entry.path()) {
                Ok(Some(found)) => found,
                Ok(None) => {
                    counts.add(&searcher.last());
                    return WalkState::Continue;
                }
                Err(source) => {
                    report_error(Error::Io {
                        path: entry.path().to_path_buf(),
                        source,
                    });
                    return WalkState::Continue;
                }
            };
            counts.matched.fetch_add(1, Ordering::Relaxed);
            counts.add(&found.stats);
            match on_hit(&FileHit {
                path: entry.path(),
                lines: found.lines,
                binary_offset: found.binary_offset,
                stats: found.stats,
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
        bytes_searched: counts.bytes.into_inner(),
        search_time: Duration::from_nanos(counts.nanos.into_inner()),
    })
}

#[derive(Default)]
struct Counts {
    searched: AtomicU64,
    matched: AtomicU64,
    errors: AtomicU64,
    bytes: AtomicU64,
    nanos: AtomicU64,
}

impl Counts {
    fn add(&self, stats: &FileStats) {
        self.bytes
            .fetch_add(stats.bytes_searched, Ordering::Relaxed);
        let nanos = u64::try_from(stats.elapsed.as_nanos()).unwrap_or(u64::MAX);
        self.nanos.fetch_add(nanos, Ordering::Relaxed);
    }
}

/// ripgrep's default thread count: logical CPUs, capped at 12.
fn default_threads() -> usize {
    std::thread::available_parallelism()
        .map_or(1, std::num::NonZero::get)
        .min(12)
}

/// The walk ripgrep does for these roots, filter and rules, and the errors of `--ignore-file`
/// files it couldn't read.
fn walk_builder(
    roots: &[std::path::PathBuf],
    filter: Filter,
    rules: &Rules,
    options: Options,
) -> (WalkBuilder, Vec<ignore::Error>) {
    let mut roots = roots.iter();
    let mut builder =
        WalkBuilder::new(roots.next().map_or_else(|| Path::new("."), |p| p.as_path()));
    for root in roots {
        builder.add(root);
    }
    builder
        .standard_filters(filter.respect_ignore_files)
        .hidden(!filter.include_hidden)
        .follow_links(filter.follow_links)
        .threads(options.thread_count());
    let ignore = &rules.ignore_files;
    if filter.respect_ignore_files {
        // As ripgrep maps its `--no-ignore-*` flags onto the walker.
        let vcs = filter.respect_vcs_ignores;
        builder
            .parents(!ignore.no_parent)
            .ignore(!ignore.no_dot)
            .git_global(vcs && !ignore.no_global)
            .git_ignore(vcs)
            .git_exclude(vcs && !ignore.no_exclude);
        if !ignore.no_dot {
            builder.add_custom_ignore_filename(".rgignore");
        }
    }
    let unreadable = ignore
        .extra
        .iter()
        .filter_map(|path| builder.add_ignore(path))
        .collect();
    if let Some(globs) = &rules.globs {
        builder.overrides(globs.clone());
    }
    if let Some(types) = &rules.types {
        builder.types(types.clone());
    }
    (builder, unreadable)
}

fn searcher(filter: Filter, output: Output) -> Searcher {
    // Files found by walking a directory get ripgrep's "implicit" binary handling:
    // stop at the first NUL byte. `--binary` (-uuu) searches them, converting NULs.
    let binary = if filter.include_binary {
        BinaryDetection::convert(b'\x00')
    } else {
        BinaryDetection::quit(b'\x00')
    };
    // ripgrep gives the searcher its context and `-m` in every output mode, so they shape
    // counts too (`-c -m1 -A2` counts a match inside the trailing context).
    SearcherBuilder::new()
        .line_number(true)
        .binary_detection(binary)
        .before_context(output.before)
        .after_context(output.after)
        .max_matches(output.max_count)
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
    output: Output,
    matcher: &'a RegexMatcher,
    lines: &'a mut Vec<LineHit>,
    matched: bool,
    binary_offset: Option<u64>,
    stats: FileStats,
}

/// What a [`Collect`] found, once the search is over.
#[derive(Clone, Copy)]
struct Done {
    matched: bool,
    binary_offset: Option<u64>,
    stats: FileStats,
}

impl<'a> Collect<'a> {
    fn new(output: Output, matcher: &'a RegexMatcher, lines: &'a mut Vec<LineHit>) -> Self {
        Self {
            output,
            matcher,
            lines,
            matched: false,
            binary_offset: None,
            stats: FileStats::default(),
        }
    }

    fn done(self) -> Done {
        Done {
            matched: self.matched,
            binary_offset: self.binary_offset,
            stats: self.stats,
        }
    }
}

/// Where `matcher` matches in `line`, as ripgrep's printers find them: the search leaves
/// out the line's `\n`, and an empty match at the very end of a line without one doesn't
/// count.
fn positions(matcher: &RegexMatcher, line: &[u8]) -> io::Result<Vec<(usize, usize)>> {
    let text = line.strip_suffix(b"\n").unwrap_or(line);
    let mut out = Vec::new();
    matcher
        .find_iter(text, |m| {
            out.push((m.start(), m.end()));
            true
        })
        .map_err(io::Error::other)?;
    if out.last().is_some_and(|&(s, e)| s == e && s >= line.len()) {
        out.pop();
    }
    Ok(out)
}

impl Sink for Collect<'_> {
    type Error = std::io::Error;

    fn matched(&mut self, _: &Searcher, m: &SinkMatch<'_>) -> Result<bool, Self::Error> {
        self.matched = true;
        self.stats.matched_lines += 1;
        match self.output.mode {
            Mode::Files => return Ok(false), // one match is enough
            Mode::Count => {}
            Mode::CountMatches => {
                self.stats.matches += positions(self.matcher, m.bytes())?.len() as u64;
            }
            Mode::Lines => {
                let matches = if self.output.positions {
                    positions(self.matcher, m.bytes())?
                } else {
                    Vec::new()
                };
                self.stats.matches += matches.len() as u64;
                self.lines.push(LineHit {
                    line_number: m.line_number().unwrap_or(0),
                    offset: m.absolute_byte_offset(),
                    bytes: m.bytes().to_vec(),
                    context: false,
                    matches,
                });
            }
        }
        Ok(true)
    }

    fn context(&mut self, _: &Searcher, c: &SinkContext<'_>) -> Result<bool, Self::Error> {
        if self.output.mode == Mode::Lines {
            self.lines.push(LineHit {
                line_number: c.line_number().unwrap_or(0),
                offset: c.absolute_byte_offset(),
                bytes: c.bytes().to_vec(),
                context: true,
                matches: Vec::new(),
            });
        }
        Ok(true)
    }

    fn finish(&mut self, _: &Searcher, finish: &SinkFinish) -> Result<(), Self::Error> {
        self.stats.bytes_searched = finish.byte_count();
        Ok(())
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
        files_with(scope, &Rules::default(), query)
    }

    fn files_with(scope: &Scope, rules: &Rules, query: &Query) -> BTreeSet<PathBuf> {
        let out = Mutex::new(BTreeSet::new());
        scan(
            scope,
            rules,
            query,
            Mode::Files.into(),
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
            &Rules::default(),
            &Query::literal("needle"),
            Mode::Lines.into(),
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
    fn a_long_line_earlier_does_not_hide_a_match_before_a_late_nul() {
        // A 300 KB line grows grep-searcher's line buffer well past 64 KiB. Reused as is,
        // the next file's NUL (200 KB in) would land in its first buffer fill, and the
        // searcher would quit before searching the match on line 1.
        let long_line = vec![b'y'; 300_000];
        let mut late = b"needle first\n".to_vec();
        late.extend(std::iter::repeat_n(b"short line\n".as_slice(), 20_000).flatten());
        late.extend_from_slice(b"\0 needle after\n");

        let matcher = build_matcher(&Query::literal("needle")).unwrap();
        let mut searcher = FileSearcher::new(Filter::default(), Mode::Lines);
        assert!(
            searcher
                .search_bytes(&matcher, &long_line)
                .unwrap()
                .is_none()
        );
        let found = searcher.search_bytes(&matcher, &late).unwrap().unwrap();
        assert_eq!(found.lines.len(), 1);
        assert!(found.binary_offset.is_some());
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
            &Rules::default(),
            &Query::literal("needle"),
            Mode::Lines.into(),
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
                LineHit::matched(1, 0, b"one needle\n".to_vec()),
                LineHit::matched(3, 15, b"three needle".to_vec()),
            ]
        );
    }

    /// One file's reported lines as (number, offset, context, text, positions), and its stats.
    type Report = (
        Vec<(u64, u64, bool, String, Vec<(usize, usize)>)>,
        FileStats,
    );

    fn report(output: Output, pattern: &str, text: &str) -> Report {
        let matcher = build_matcher(&Query::regex(pattern)).unwrap();
        let mut searcher = FileSearcher::new(Filter::default(), output);
        let Some(found) = searcher.search_bytes(&matcher, text.as_bytes()).unwrap() else {
            return (Vec::new(), searcher.last());
        };
        let lines = found
            .lines
            .iter()
            .map(|l| {
                let text = String::from_utf8(l.bytes.clone()).unwrap();
                (l.line_number, l.offset, l.context, text, l.matches.clone())
            })
            .collect();
        (lines, found.stats)
    }

    #[test]
    fn context_lines_come_with_their_offsets() {
        let output = Output {
            mode: Mode::Lines,
            before: 1,
            after: 1,
            ..Output::default()
        };
        let (lines, stats) = report(output, "needle", "a\nb needle\nc\nd\ne\nf needle\n");
        let numbers: Vec<(u64, u64, bool)> = lines.iter().map(|l| (l.0, l.1, l.2)).collect();
        assert_eq!(
            numbers,
            [
                (1, 0, true),
                (2, 2, false),
                (3, 11, true),
                (5, 15, true),
                (6, 17, false)
            ]
        );
        assert_eq!((stats.matched_lines, stats.bytes_searched), (2, 26));
    }

    /// `rg -m1 -A2` on these lines prints line 2 as a match: the trailing context of the
    /// last allowed match still reports matches, and `-c` counts them.
    #[test]
    fn max_count_keeps_trailing_context_and_its_matches() {
        let text = "one needle\ntwo needle\nthree\nfour needle\n";
        let output = Output {
            mode: Mode::Lines,
            after: 2,
            max_count: Some(1),
            ..Output::default()
        };
        let (lines, _) = report(output, "needle", text);
        let kinds: Vec<(u64, bool)> = lines.iter().map(|l| (l.0, l.2)).collect();
        assert_eq!(kinds, [(1, false), (2, false), (3, true)]);
        let count = Output {
            mode: Mode::Count,
            ..output
        };
        assert_eq!(report(count, "needle", text).1.matched_lines, 2);
        let none = Output {
            max_count: Some(0),
            ..output
        };
        assert_eq!(report(none, "needle", text).0.len(), 0);
    }

    /// The positions `rg --json` reports and the matches `rg --count-matches` counts,
    /// empty matches included, as measured with ripgrep 15.
    #[test]
    fn match_positions_and_counts_follow_ripgrep() {
        let output = Output {
            mode: Mode::Lines,
            positions: true,
            ..Output::default()
        };
        let (lines, stats) = report(output, "x*", "abc\nxx\n");
        assert_eq!(lines[0].4, [(0, 0), (1, 1), (2, 2), (3, 3)]);
        assert_eq!(lines[1].4, [(0, 2)]);
        assert_eq!(stats.matches, 5);
        let (lines, _) = report(output, "x*", "abc");
        assert_eq!(
            lines[0].4,
            [(0, 0), (1, 1), (2, 2)],
            "no empty match at the end"
        );
        let (lines, _) = report(output, "x*", "ab\r\n");
        assert_eq!(lines[0].4.len(), 4, "only the newline is left out");
        let count = Output::from(Mode::CountMatches);
        assert_eq!(report(count, "x*", "abc\nxx\n").1.matches, 5);
        assert_eq!(report(count, "$", "abc\nxx\n").1.matches, 2);
    }

    fn select(globs: &[&str], types: &[(&str, bool)]) -> Select {
        Select {
            globs: globs.iter().map(|g| (*g).to_owned()).collect(),
            iglobs: Vec::new(),
            types: types
                .iter()
                .map(|&(name, negated)| mtseek_core::TypeRule {
                    name: name.to_owned(),
                    negated,
                })
                .collect(),
            ignore_files: IgnoreFiles::default(),
        }
    }

    /// The corpus probed against ripgrep 15 to pin down how globs and types meet ignore
    /// files and hidden names.
    #[test]
    fn globs_and_types_override_ignore_files_and_hidden_names_as_in_ripgrep() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        for d in [".git", "sub", ".hid", "ign"] {
            fs::create_dir_all(root.join(d)).unwrap();
        }
        fs::write(root.join(".gitignore"), "ign/\n*.log\n").unwrap();
        for f in [
            "a.txt",
            ".h.txt",
            "sub/b.txt",
            ".hid/c.txt",
            "ign/d.txt",
            "e.log",
            "sub/x.rs",
            ".y.rs",
        ] {
            fs::write(root.join(f), "needle\n").unwrap();
        }
        let scope = Scope::new([root]);
        let q = Query::literal("needle");
        let with = |globs: &[&str], types: &[(&str, bool)]| {
            let rules = Rules::new(&select(globs, types), root).unwrap();
            files_with(&scope, &rules, &q)
        };
        assert_eq!(
            with(&["*.txt"], &[]),
            set(&[".h.txt", "a.txt", "sub/b.txt"])
        );
        assert_eq!(with(&["*.log"], &[]), set(&["e.log"]));
        assert_eq!(with(&["!sub"], &[]), set(&["a.txt"]));
        assert_eq!(with(&["!*.txt"], &[]), set(&["sub/x.rs"]));
        assert_eq!(with(&[], &[("rust", false)]), set(&[".y.rs", "sub/x.rs"]));
        assert_eq!(with(&[], &[("rust", true)]), set(&["a.txt", "sub/b.txt"]));
        assert_eq!(
            with(&[".hid", "*.txt"], &[]),
            set(&[".h.txt", ".hid/c.txt", "a.txt", "sub/b.txt"])
        );
        assert_eq!(with(&["*.txt"], &[("rust", false)]), with(&["*.txt"], &[]));
        assert_eq!(
            with(&["*.txt", "!a.txt"], &[]),
            set(&[".h.txt", "sub/b.txt"])
        );

        let widening = |globs: &[&str], types: &[(&str, bool)]| {
            Rules::new(&select(globs, types), root).unwrap().widening()
        };
        assert_eq!(widening(&["!x"], &[("rust", true)]), Widening::None);
        assert_eq!(widening(&["!x"], &[("rust", false)]), Widening::Hidden);
        assert_eq!(widening(&["x"], &[]), Widening::All);
        let err = Rules::new(&select(&[], &[("nope", false)]), root).unwrap_err();
        assert_eq!(err.to_string(), "unrecognized file type: nope");
        assert!(Rules::new(&select(&["["], &[]), root).is_err());
    }

    #[test]
    fn invalid_regex_is_an_error_not_a_panic() {
        let err = build_matcher(&Query::regex("(unclosed")).unwrap_err();
        assert!(matches!(err, Error::InvalidPattern(_)));
    }
}
