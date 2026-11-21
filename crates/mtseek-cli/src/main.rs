//! `mtseek`: the command-line interface.
//!
//! The flags and output format follow ripgrep (see `docs/adr/0005-ripgrep-compatible-contract.md`),
//! so scripts, editors and AI agents that already speak `rg` work unchanged.
//! Searches run on `mtseek-engine`: content queries through the content index of a coverage
//! root when one covers them (built with `--index`, caught up from the change journal on
//! every run; ADR 0010, 0011, 0012), on the reference scanner otherwise.

use std::io::{self, BufWriter, IsTerminal, Write};
use std::ops::ControlFlow;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use clap::{ArgAction, Parser};
use mtseek_core::{CaseMode, Error, Filter, PatternKind, Query, Scope};
use mtseek_engine::{Engine, IndexUse, NamesRequest, Report, SearchRequest};
use mtseek_walk::{FileHit, Mode, Summary};

/// Instant full-disk search. Takes ripgrep's flags and prints ripgrep's output format.
#[derive(Debug, Parser)]
#[command(name = "mtseek", version, max_term_width = 100)]
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
    #[arg(short = 'l', long)]
    files_with_matches: bool,

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
        conflicts_with_all = ["fixed_strings", "files_with_matches", "line_number", "unrestricted", "hidden", "no_ignore", "follow", "index", "reindex", "no_index"]
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
}

impl Args {
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

    fn search_request(&self) -> SearchRequest {
        let scope = self.scope();
        SearchRequest {
            query: self.query(),
            roots: scope.roots,
            filter: scope.filter,
            mode: if self.files_with_matches {
                Mode::Files
            } else {
                Mode::Lines
            },
            threads: self.threads,
            index: self.index_use(),
            cwd: None,
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

#[derive(Debug, Default)]
struct Format {
    line_number: bool,
    with_filename: bool,
}

fn main() -> ExitCode {
    let args = Args::parse();
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

fn run(args: &Args) -> mtseek_core::Result<ExitCode> {
    let request = args.search_request();
    let printer = Printer::new(request.mode, args.format(&args.scope()));
    let mut engine = Engine::direct(Engine::default_cache_dir());
    let outcome = engine.search(&request, &printer);
    // Flush what was found even if the search then failed.
    let finished = printer.finish();
    let outcome = outcome?;
    if !finished? {
        return Ok(ExitCode::SUCCESS);
    }
    if args.stats {
        print_stats(&outcome.stats);
    }
    Ok(exit_code(&outcome.summary))
}

/// `mtseek --names`: file-name search.
fn run_names(args: &Args) -> mtseek_core::Result<ExitCode> {
    let printer = Printer::new(Mode::Files, Format::default());
    let mut engine = Engine::direct(Engine::default_cache_dir());
    let found = engine.names(&args.names_request(), &printer)?;
    let mut stats = found.stats;
    print_paths(&found.paths)?;
    // Saved after answering, so the caller never waits for it.
    stats.extend(engine.save(Duration::ZERO));
    if args.stats {
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
    out: Mutex<BufWriter<io::Stdout>>,
    write_error: Mutex<Option<io::Error>>,
    mode: Mode,
    format: Format,
}

impl Printer {
    fn new(mode: Mode, format: Format) -> Self {
        Self {
            out: Mutex::new(BufWriter::with_capacity(64 * 1024, io::stdout())),
            write_error: Mutex::new(None),
            mode,
            format,
        }
    }

    /// Flushes stdout. Returns `Ok(false)` if the reader went away: a closed pipe
    /// (`mtseek foo | head`) is a normal way to stop, not a failure.
    fn finish(self) -> mtseek_core::Result<bool> {
        let flushed = self
            .out
            .into_inner()
            .unwrap_or_else(PoisonError::into_inner)
            .flush();
        let write_error = self
            .write_error
            .into_inner()
            .unwrap_or_else(PoisonError::into_inner);
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
        match write_hit(&mut *out, hit, self.mode, &self.format) {
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

fn write_hit(
    out: &mut impl Write,
    hit: &FileHit<'_>,
    mode: Mode,
    format: &Format,
) -> io::Result<()> {
    let path = hit.path.to_string_lossy();
    if mode == Mode::Files {
        out.write_all(path.as_bytes())?;
        return out.write_all(b"\n");
    }
    for line in hit.lines {
        if format.with_filename {
            out.write_all(path.as_bytes())?;
            out.write_all(b":")?;
        }
        if format.line_number {
            write!(out, "{}:", line.line_number)?;
        }
        out.write_all(&line.bytes)?;
        if !line.bytes.ends_with(b"\n") {
            out.write_all(b"\n")?;
        }
    }
    if let (Some(offset), false) = (hit.binary_offset, hit.lines.is_empty()) {
        // ripgrep's exact wording, so line counts and parsers agree.
        writeln!(
            out,
            r#"{path}: WARNING: stopped searching binary file after match (found "\0" byte around offset {offset})"#
        )?;
    }
    Ok(())
}

fn print_paths(paths: &[PathBuf]) -> mtseek_core::Result<()> {
    let mut out = BufWriter::with_capacity(64 * 1024, io::stdout().lock());
    let written = paths.iter().try_for_each(|path| {
        out.write_all(path.as_os_str().to_string_lossy().as_bytes())?;
        out.write_all(b"\n")
    });
    // A closed pipe (`mtseek --names x | head`) is a normal way to stop, not a failure.
    match written.and_then(|()| out.flush()) {
        Err(err) if err.kind() != io::ErrorKind::BrokenPipe => Err(Error::Io {
            path: PathBuf::from("<stdout>"),
            source: err,
        }),
        _ => Ok(()),
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
