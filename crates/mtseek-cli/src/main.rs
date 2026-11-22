//! `mtseek`: the command-line interface.
//!
//! The flags and output format follow ripgrep (see `docs/adr/0005-ripgrep-compatible-contract.md`),
//! so scripts, editors and AI agents that already speak `rg` work unchanged.
//! Searches run on `mtseek-engine`: content queries through the content index of a coverage
//! root when one covers them (built with `--index`, caught up from the change journal on
//! every run; ADR 0010, 0011, 0012), on the reference scanner otherwise.
//!
//! When the user's daemon (`mtseekd`) runs, the CLI sends it the search over its pipe and
//! prints what comes back; the daemon runs the same engine with its indexes already open
//! and current (ADR 0013). Without one, the CLI runs the engine itself ("direct mode").
//! Output is the same either way.

use std::io::{self, BufWriter, IsTerminal, Write};
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use clap::{ArgAction, ArgMatches, CommandFactory, FromArgMatches, Parser};
use mtseek_core::{
    CaseMode, Error, Filter, IgnoreFiles, PatternKind, Query, Scope, Select, TypeRule,
};
use mtseek_engine::{Engine, IndexUse, NamesRequest, Report, SearchRequest};
use mtseek_walk::{FileHit, Mode, Output, Summary};

mod daemon;
mod json;

/// Instant full-disk search. Takes ripgrep's flags and prints ripgrep's output format.
#[derive(Debug, Parser)]
#[command(
    name = "mtseek",
    version,
    max_term_width = 100,
    // As in ripgrep, a flag given twice takes its last value.
    args_override_self = true
)]
#[allow(clippy::struct_excessive_bools)] // one field per command-line flag
struct Args {
    /// A regular expression to search for, or literal text with -F.
    pattern: String,

    /// Files or directories to search [default: the current directory].
    paths: Vec<PathBuf>,

    /// Treat the pattern as literal text, not a regular expression.
    #[arg(short = 'F', long)]
    fixed_strings: bool,

    /// Match case-insensitively.
    #[arg(short = 'i', long, overrides_with_all = ["case_sensitive", "smart_case"])]
    ignore_case: bool,

    /// Match case-insensitively unless the pattern contains an uppercase letter.
    #[arg(short = 'S', long, overrides_with_all = ["case_sensitive", "ignore_case"])]
    smart_case: bool,

    /// Match case-sensitively (the default).
    #[arg(short = 's', long, overrides_with_all = ["ignore_case", "smart_case"])]
    case_sensitive: bool,

    /// Print only the paths of files with at least one match.
    #[arg(short = 'l', long, overrides_with_all = ["count", "count_matches", "json"])]
    files_with_matches: bool,

    /// Print the number of matching lines in each file with a match.
    #[arg(short = 'c', long, overrides_with_all = ["files_with_matches", "count_matches", "json"])]
    count: bool,

    /// Print the number of matches in each file with a match.
    #[arg(long, overrides_with_all = ["files_with_matches", "count", "json"])]
    count_matches: bool,

    /// Print results as JSON Lines, message for message as `rg --json` does.
    #[arg(long, overrides_with_all = ["files_with_matches", "count", "count_matches"])]
    json: bool,

    /// Show NUM lines after each match.
    #[arg(short = 'A', long, value_name = "NUM")]
    after_context: Option<usize>,

    /// Show NUM lines before each match.
    #[arg(short = 'B', long, value_name = "NUM")]
    before_context: Option<usize>,

    /// Show NUM lines before and after each match. -A and -B take precedence.
    #[arg(short = 'C', long, value_name = "NUM")]
    context: Option<usize>,

    /// Stop searching a file after NUM matching lines.
    #[arg(short = 'm', long, value_name = "NUM")]
    max_count: Option<u64>,

    /// Search only files that match GLOB, or with `!GLOB`, leave them out. Can be repeated;
    /// later globs win. A matching file is searched even if it is ignored or hidden.
    #[arg(short = 'g', long = "glob", value_name = "GLOB")]
    globs: Vec<String>,

    /// Like -g, ignoring case.
    #[arg(long = "iglob", value_name = "GLOB")]
    iglobs: Vec<String>,

    /// Search only files of type TYPE (ripgrep's built-in types: rust, py, js, ...).
    #[arg(short = 't', long = "type", value_name = "TYPE")]
    types: Vec<String>,

    /// Leave out files of type TYPE.
    #[arg(short = 'T', long = "type-not", value_name = "TYPE")]
    types_not: Vec<String>,

    /// `-t` and `-T` in command-line order, which decides between them.
    #[arg(skip)]
    type_rules: Vec<TypeRule>,

    /// Show line numbers [default: on when printing to a terminal].
    #[arg(short = 'n', long, overrides_with = "no_line_number")]
    line_number: bool,

    /// Hide line numbers.
    #[arg(short = 'N', long, overrides_with = "line_number")]
    no_line_number: bool,

    /// Print the file path on every matching line [default: on, unless searching a single file].
    #[arg(short = 'H', long, overrides_with = "no_filename")]
    with_filename: bool,

    /// Never print file paths in front of matching lines.
    #[arg(short = 'I', long, overrides_with = "with_filename")]
    no_filename: bool,

    /// Print the path on each line instead of grouping under a heading (the only layout mtseek has).
    #[arg(long, hide = true)]
    no_heading: bool,

    /// Reduce filtering: -u skips ignore files, -uu also searches hidden files, -uuu also binary files.
    #[arg(short = 'u', long = "unrestricted", action = ArgAction::Count)]
    unrestricted: u8,

    /// Search hidden files and directories.
    #[arg(long = "hidden", short = '.')]
    hidden: bool,

    /// Don't honour .gitignore, .ignore or .rgignore files.
    #[arg(long)]
    no_ignore: bool,

    /// Don't honour .gitignore, .git/info/exclude or git's global excludes file; .ignore and
    /// .rgignore still apply.
    #[arg(long)]
    no_ignore_vcs: bool,

    /// Don't honour .ignore or .rgignore files.
    #[arg(long)]
    no_ignore_dot: bool,

    /// Don't honour ignore files in folders above the one searched.
    #[arg(long)]
    no_ignore_parent: bool,

    /// Don't honour git's global excludes file.
    #[arg(long)]
    no_ignore_global: bool,

    /// Don't honour .git/info/exclude.
    #[arg(long)]
    no_ignore_exclude: bool,

    /// Also leave out what this file lists, in .gitignore syntax. Can be repeated. Applies
    /// even with -u, as in ripgrep.
    #[arg(long = "ignore-file", value_name = "PATH")]
    ignore_files: Vec<PathBuf>,

    /// Don't honour files given with --ignore-file.
    #[arg(long)]
    no_ignore_files: bool,

    /// Follow symbolic links and junctions.
    #[arg(short = 'L', long)]
    follow: bool,

    /// Number of worker threads [default: logical CPUs, at most 12].
    #[arg(short = 'j', long, default_value_t = 0, hide_default_value = true)]
    threads: usize,

    /// Accepted for ripgrep compatibility. mtseek has no config file.
    #[arg(long, hide = true)]
    no_config: bool,

    /// Search file and folder names instead of contents (like Everything). Space-separated
    /// terms must all appear in the name. Smart case by default. Covers every file under the
    /// paths, ignored and hidden ones included, and prints absolute paths. Reads the NTFS
    /// master file table when run as administrator; otherwise walks directories.
    #[arg(
        long,
        conflicts_with_all = [
            "fixed_strings", "files_with_matches", "line_number", "unrestricted", "hidden",
            "no_ignore", "follow", "index", "reindex", "no_index", "count", "count_matches",
            "json", "after_context", "before_context", "context", "max_count", "globs",
            "iglobs", "types", "types_not", "no_ignore_vcs", "no_ignore_dot",
            "no_ignore_parent", "no_ignore_global", "no_ignore_exclude", "ignore_files",
            "no_ignore_files",
        ]
    )]
    names: bool,

    /// Build a content index of the paths (each a directory) with these filters, kept in the
    /// cache, and search it. Once a scope has an index, every search of it uses the index,
    /// caught up from the NTFS change journal first, so results stay current.
    #[arg(long, conflicts_with = "no_index")]
    index: bool,

    /// Rebuild the content index, then search it (implies --index).
    #[arg(long, conflicts_with = "no_index")]
    reindex: bool,

    /// Scan the files even if the paths have a content index.
    #[arg(long)]
    no_index: bool,

    /// With --names or --index: print timings and index size to stderr.
    #[arg(long)]
    stats: bool,

    /// Search in this process even if the daemon (mtseekd) is running.
    #[arg(long, conflicts_with = "daemon")]
    no_daemon: bool,

    /// Fail unless the daemon (mtseekd) answers, instead of searching in this process.
    #[arg(long)]
    daemon: bool,
}

impl Args {
    /// Parses the command line.
    fn parse_all() -> Self {
        let matches = Self::command().get_matches();
        let mut args = Self::from_arg_matches(&matches).unwrap_or_else(|e| e.exit());
        args.type_rules = type_rules(&matches);
        args
    }

    fn query(&self) -> Query {
        let kind = if self.fixed_strings {
            PatternKind::Literal
        } else {
            PatternKind::Regex
        };
        let case = if self.ignore_case {
            CaseMode::Insensitive
        } else if self.smart_case {
            CaseMode::Smart
        } else {
            CaseMode::Sensitive
        };
        Query {
            pattern: self.pattern.clone(),
            kind,
            case,
        }
    }

    fn scope(&self) -> Scope {
        let mut filter = Filter::unrestricted(self.unrestricted);
        filter.respect_ignore_files &= !self.no_ignore;
        filter.include_hidden |= self.hidden;
        filter.follow_links = self.follow;
        let roots = if self.paths.is_empty() {
            vec![PathBuf::from(".")]
        } else {
            self.paths.clone()
        };
        Scope { roots, filter }
    }

    fn index_use(&self) -> IndexUse {
        if self.no_index {
            IndexUse::Off
        } else if self.reindex {
            IndexUse::Rebuild
        } else if self.index {
            IndexUse::Build
        } else {
            IndexUse::Auto
        }
    }

    fn output(&self) -> Output {
        let mode = if self.files_with_matches {
            Mode::Files
        } else if self.count {
            Mode::Count
        } else if self.count_matches {
            Mode::CountMatches
        } else {
            Mode::Lines
        };
        Output {
            mode,
            before: self.before_context.or(self.context).unwrap_or(0),
            after: self.after_context.or(self.context).unwrap_or(0),
            max_count: self.max_count,
            positions: self.json,
        }
    }

    fn search_request(&self) -> SearchRequest {
        let scope = self.scope();
        SearchRequest {
            query: self.query(),
            roots: scope.roots,
            filter: scope.filter,
            select: Select {
                globs: self.globs.clone(),
                iglobs: self.iglobs.clone(),
                types: self.type_rules.clone(),
                ignore_files: IgnoreFiles {
                    no_vcs: self.no_ignore_vcs,
                    no_dot: self.no_ignore_dot,
                    no_parent: self.no_ignore_parent,
                    no_global: self.no_ignore_global,
                    no_exclude: self.no_ignore_exclude,
                    extra: if self.no_ignore_files {
                        Vec::new()
                    } else {
                        self.ignore_files.clone()
                    },
                },
            },
            output: self.output(),
            threads: self.threads,
            index: self.index_use(),
            cwd: None,
        }
    }

    fn style(&self, output: &Output) -> Style {
        if self.json {
            return Style::Json;
        }
        match output.mode {
            Mode::Files => Style::Paths,
            Mode::Count => Style::Count,
            Mode::CountMatches => Style::CountMatches,
            Mode::Lines => Style::Lines {
                context: output.context(),
            },
        }
    }

    fn names_request(&self) -> NamesRequest {
        let case = if self.ignore_case {
            CaseMode::Insensitive
        } else if self.case_sensitive {
            CaseMode::Sensitive
        } else {
            CaseMode::Smart
        };
        NamesRequest {
            pattern: self.pattern.clone(),
            case,
            roots: self.paths.clone(),
            threads: self.threads,
            cwd: None,
        }
    }

    fn format(&self, scope: &Scope) -> Format {
        let single_file = scope.roots.len() == 1 && scope.roots[0].is_file();
        Format {
            line_number: self.line_number || (!self.no_line_number && io::stdout().is_terminal()),
            with_filename: self.with_filename || (!self.no_filename && !single_file),
        }
    }
}

/// `-t` and `-T` in the order they were given.
fn type_rules(matches: &ArgMatches) -> Vec<TypeRule> {
    let mut rules: Vec<(usize, TypeRule)> = Vec::new();
    for (id, negated) in [("types", false), ("types_not", true)] {
        let (Some(names), Some(at)) = (matches.get_many::<String>(id), matches.indices_of(id))
        else {
            continue;
        };
        rules.extend(at.zip(names).map(|(i, name)| {
            (
                i,
                TypeRule {
                    name: name.clone(),
                    negated,
                },
            )
        }));
    }
    rules.sort_by_key(|&(i, _)| i);
    rules.into_iter().map(|(_, rule)| rule).collect()
}

#[derive(Debug, Default)]
struct Format {
    line_number: bool,
    with_filename: bool,
}

/// How a content search's results are printed, as ripgrep prints them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Style {
    /// One path per matching file (`-l`).
    Paths,
    /// Matching lines per file (`-c`).
    Count,
    /// Matches per file (`--count-matches`).
    CountMatches,
    /// Matching lines; with context lines and `--` between groups if `context`.
    Lines { context: bool },
    /// JSON Lines (`--json`).
    Json,
}

type Failure = Box<dyn std::error::Error>;

fn main() -> ExitCode {
    let args = Args::parse_all();
    let result = if args.names {
        run_names(&args)
    } else {
        run(&args)
    };
    match result {
        Ok(code) => code,
        Err(err) => {
            let _ = writeln!(io::stderr(), "mtseek: {err}");
            ExitCode::from(2)
        }
    }
}

fn run(args: &Args) -> Result<ExitCode, Failure> {
    let request = args.search_request();
    let printer = Printer::new(args.style(&request.output), args.format(&args.scope()));
    let started = Instant::now();
    let direct = |by: String| {
        Engine::direct(Engine::default_cache_dir())
            .search(&request, &printer)
            .map(|outcome| (Some(outcome), by))
            .map_err(Failure::from)
    };
    let answered = match daemon::connect(args)? {
        Some(mut client) => match daemon::search(&mut client, &request, &printer) {
            Ok(daemon::Answer::Done(outcome)) => {
                Ok((outcome, daemon::answered_by(&client, started)))
            }
            Ok(daemon::Answer::Declined(why)) => direct(daemon::declined_by(&client, &why)),
            Err(err) => Err(err),
        },
        None => direct("answered in this process".to_owned()),
    };
    // Flush what was found even if the search then failed.
    let summary = match &answered {
        Ok((Some(outcome), _)) => Some(outcome.summary),
        _ => None,
    };
    let finished = printer.finish(summary.as_ref());
    let (outcome, by) = answered?;
    let Some(mut outcome) = outcome.filter(|_| finished.as_ref().is_ok_and(|&f| f)) else {
        finished?;
        return Ok(ExitCode::SUCCESS); // the reader went away (`mtseek x | head`)
    };
    if args.stats {
        outcome.stats.push(by);
        print_stats(&outcome.stats);
    }
    Ok(exit_code(&outcome.summary))
}

/// `mtseek --names`: file-name search.
fn run_names(args: &Args) -> Result<ExitCode, Failure> {
    let request = args.names_request();
    let started = Instant::now();
    let mut by = "answered in this process".to_owned();
    if let Some(mut client) = daemon::connect(args)? {
        match daemon::names(&mut client, &request)? {
            Ok((matched, mut stats)) => {
                if args.stats {
                    stats.push(daemon::answered_by(&client, started));
                    print_stats(&stats);
                }
                return Ok(if matched == 0 {
                    ExitCode::from(1)
                } else {
                    ExitCode::SUCCESS
                });
            }
            Err(why) => by = daemon::declined_by(&client, &why),
        }
    }
    let printer = Printer::new(Style::Paths, Format::default());
    let mut engine = Engine::direct(Engine::default_cache_dir());
    let found = engine.names(&request, &printer)?;
    let mut stats = found.stats;
    let mut out = PathWriter::new();
    for path in &found.paths {
        out.write(path);
    }
    out.finish()?;
    // Saved after answering, so the caller never waits for it.
    stats.extend(engine.save(Duration::ZERO));
    if args.stats {
        stats.push(by);
        print_stats(&stats);
    }
    Ok(if found.paths.is_empty() {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    })
}

/// ripgrep's exit codes: 2 if anything failed, else 0 on a match, 1 on none.
fn exit_code(summary: &Summary) -> ExitCode {
    if summary.errors > 0 {
        ExitCode::from(2)
    } else if summary.files_matched > 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    }
}

/// Prints what a content search finds, as ripgrep would.
struct Printer {
    out: Mutex<Out>,
    write_error: Mutex<Option<io::Error>>,
    style: Style,
    format: Format,
    started: Instant,
}

/// Stdout and what has been printed to it.
struct Out {
    writer: BufWriter<io::Stdout>,
    /// Whether any file's results have been printed: with context, ripgrep puts `--`
    /// between files.
    printed: bool,
    /// Totals for the JSON summary.
    json: json::Totals,
}

impl Printer {
    fn new(style: Style, format: Format) -> Self {
        Self {
            out: Mutex::new(Out {
                writer: BufWriter::with_capacity(64 * 1024, io::stdout()),
                printed: false,
                json: json::Totals::default(),
            }),
            write_error: Mutex::new(None),
            style,
            format,
            started: Instant::now(),
        }
    }

    /// Prints the JSON summary for a search that finished with `summary`, and flushes
    /// stdout. Returns `Ok(false)` if the reader went away: a closed pipe
    /// (`mtseek foo | head`) is a normal way to stop, not a failure.
    fn finish(self, summary: Option<&Summary>) -> mtseek_core::Result<bool> {
        let mut out = self
            .out
            .into_inner()
            .unwrap_or_else(PoisonError::into_inner);
        let mut write_error = self
            .write_error
            .into_inner()
            .unwrap_or_else(PoisonError::into_inner);
        if let (Style::Json, Some(summary), None) = (self.style, summary, &write_error) {
            let elapsed = self.started.elapsed();
            if let Err(err) = json::write_summary(&mut out.writer, &out.json, summary, elapsed) {
                write_error = Some(err);
            }
        }
        let flushed = out.writer.flush();
        match write_error.or_else(|| flushed.err()) {
            None => Ok(true),
            Some(err) if err.kind() == io::ErrorKind::BrokenPipe => Ok(false),
            Some(err) => Err(Error::Io {
                path: PathBuf::from("<stdout>"),
                source: err,
            }),
        }
    }
}

impl Report for Printer {
    fn hit(&self, hit: &FileHit<'_>) -> ControlFlow<()> {
        let mut out = self.out.lock().unwrap_or_else(PoisonError::into_inner);
        let out = &mut *out;
        let written = match self.style {
            Style::Json => json::write_file(&mut out.writer, hit, &mut out.json),
            style => write_hit(out, hit, style, &self.format),
        };
        match written {
            Ok(()) => ControlFlow::Continue(()),
            Err(err) => {
                *self
                    .write_error
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner) = Some(err);
                ControlFlow::Break(())
            }
        }
    }

    fn error(&self, err: &Error) {
        let _ = writeln!(io::stderr(), "mtseek: {err}");
    }

    fn note(&self, message: &str) {
        hint(message);
    }
}

/// Prints one file's results in a text style.
fn write_hit(out: &mut Out, hit: &FileHit<'_>, style: Style, format: &Format) -> io::Result<()> {
    let path = hit.path.to_string_lossy();
    let w = &mut out.writer;
    let count = match style {
        Style::Paths => {
            w.write_all(path.as_bytes())?;
            return w.write_all(b"\n");
        }
        Style::Count => Some(hit.stats.matched_lines),
        Style::CountMatches => Some(hit.stats.matches),
        Style::Lines { .. } | Style::Json => None,
    };
    if let Some(count) = count {
        if format.with_filename {
            w.write_all(path.as_bytes())?;
            w.write_all(b":")?;
        }
        return writeln!(w, "{count}");
    }
    let context = matches!(style, Style::Lines { context: true });
    if context && out.printed {
        w.write_all(b"--\n")?;
    }
    out.printed = true;
    let mut previous: Option<u64> = None;
    for line in hit.lines {
        // ripgrep marks a gap between groups of lines with `--`.
        if context && previous.is_some_and(|p| line.line_number > p + 1) {
            w.write_all(b"--\n")?;
        }
        previous = Some(line.line_number);
        let sep: &[u8] = if line.context { b"-" } else { b":" };
        if format.with_filename {
            w.write_all(path.as_bytes())?;
            w.write_all(sep)?;
        }
        if format.line_number {
            write!(w, "{}", line.line_number)?;
            w.write_all(sep)?;
        }
        w.write_all(&line.bytes)?;
        if !line.bytes.ends_with(b"\n") {
            w.write_all(b"\n")?;
        }
    }
    let out = w;
    if let (Some(offset), false) = (hit.binary_offset, hit.lines.is_empty()) {
        // ripgrep's exact wording, so line counts and parsers agree.
        writeln!(
            out,
            r#"{path}: WARNING: stopped searching binary file after match (found "\0" byte around offset {offset})"#
        )?;
    }
    Ok(())
}

/// Prints `--names` results, one path per line.
struct PathWriter {
    out: BufWriter<io::StdoutLock<'static>>,
    error: Option<io::Error>,
}

impl PathWriter {
    fn new() -> Self {
        Self {
            out: BufWriter::with_capacity(64 * 1024, io::stdout().lock()),
            error: None,
        }
    }

    /// Prints `path`. `false` once stdout has failed: stop.
    fn write(&mut self, path: &Path) -> bool {
        if self.error.is_some() {
            return false;
        }
        let written = self
            .out
            .write_all(path.as_os_str().to_string_lossy().as_bytes())
            .and_then(|()| self.out.write_all(b"\n"));
        if let Err(err) = written {
            self.error = Some(err);
            return false;
        }
        true
    }

    /// Flushes. A closed pipe (`mtseek --names x | head`) is a normal way to stop.
    fn finish(mut self) -> mtseek_core::Result<()> {
        let flushed = self.out.flush();
        match self.error.take().or_else(|| flushed.err()) {
            Some(err) if err.kind() != io::ErrorKind::BrokenPipe => Err(Error::Io {
                path: PathBuf::from("<stdout>"),
                source: err,
            }),
            _ => Ok(()),
        }
    }
}

fn print_stats(lines: &[String]) {
    let mut err = io::stderr().lock();
    for line in lines {
        let _ = writeln!(err, "{line}");
    }
}

/// A one-line note on stderr, only when a person is watching (never pollutes pipes or agents).
fn hint(message: &str) {
    if io::stderr().is_terminal() {
        let _ = writeln!(io::stderr(), "mtseek: {message}");
    }
}
