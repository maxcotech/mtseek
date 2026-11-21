//! `mtseek`: the command-line interface.
//!
//! The flags and output format follow ripgrep (see `docs/adr/0005-ripgrep-compatible-contract.md`),
//! so scripts, editors and AI agents that already speak `rg` work unchanged.
//! Content queries run through the scope's content index when it has one (built with
//! `--index`, caught up from the change journal on every run; ADR 0010, 0011), and on the
//! reference scanner otherwise.

use std::io::{self, BufWriter, Write};
use std::ops::ControlFlow;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::{Mutex, PoisonError};

use clap::{ArgAction, Parser};
use mtseek_core::{CaseMode, Filter, PatternKind, Query, Scope};
use mtseek_walk::{FileHit, Mode, Options};

mod index;
mod names;

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

    fn format(&self, scope: &Scope) -> Format {
        use std::io::IsTerminal;
        let single_file = scope.roots.len() == 1 && scope.roots[0].is_file();
        Format {
            line_number: self.line_number || (!self.no_line_number && io::stdout().is_terminal()),
            with_filename: self.with_filename || (!self.no_filename && !single_file),
        }
    }
}

struct Format {
    line_number: bool,
    with_filename: bool,
}

fn main() -> ExitCode {
    let args = Args::parse();
    let result = if args.names {
        names::run(&args)
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
    let scope = args.scope();
    let query = args.query();
    let mode = if args.files_with_matches {
        Mode::Files
    } else {
        Mode::Lines
    };
    let format = args.format(&scope);

    let out = Mutex::new(BufWriter::with_capacity(64 * 1024, io::stdout()));
    let write_error = Mutex::new(None::<io::Error>);

    let on_hit = |hit: &FileHit<'_>| {
        let mut out = out.lock().unwrap_or_else(PoisonError::into_inner);
        match write_hit(&mut *out, hit, mode, &format) {
            Ok(()) => ControlFlow::Continue(()),
            Err(err) => {
                *write_error.lock().unwrap_or_else(PoisonError::into_inner) = Some(err);
                ControlFlow::Break(())
            }
        }
    };
    let on_error = |err: &mtseek_core::Error| {
        let _ = writeln!(io::stderr(), "mtseek: {err}");
    };

    let options = Options {
        threads: args.threads,
    };
    let summary = match index::search(args, &scope, &query, mode, options, &on_hit, &on_error)? {
        Some(summary) => summary,
        None => mtseek_walk::scan(&scope, &query, mode, options, &on_hit, &on_error)?,
    };

    let flushed = out
        .into_inner()
        .unwrap_or_else(PoisonError::into_inner)
        .flush();
    let write_error = write_error
        .into_inner()
        .unwrap_or_else(PoisonError::into_inner);
    if let Some(err) = write_error.or_else(|| flushed.err()) {
        // A closed pipe (`mtseek foo | head`) is a normal way to stop, not a failure.
        if err.kind() == io::ErrorKind::BrokenPipe {
            return Ok(ExitCode::SUCCESS);
        }
        return Err(mtseek_core::Error::Io {
            path: PathBuf::from("<stdout>"),
            source: err,
        });
    }

    // ripgrep's exit codes: 2 if anything failed, else 0 on a match, 1 on none.
    Ok(if summary.errors > 0 {
        ExitCode::from(2)
    } else if summary.files_matched > 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    })
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
