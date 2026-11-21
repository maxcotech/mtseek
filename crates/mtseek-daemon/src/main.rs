//! `mtseekd`: the mtseek daemon (ADR 0013).
//!
//! `mtseekd --console` runs it in the foreground as the current user, until `mtseekd --stop`
//! or Ctrl+C. While it runs, `mtseek` answers through it: indexes stay open and current, so
//! a search skips opening and catching up. Service mode (one daemon per machine, run as
//! `LocalSystem`) comes with milestone M3c.

use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use clap::Parser;

/// Keeps mtseek's indexes open and current, and answers `mtseek` searches over a local
/// named pipe.
#[derive(Debug, Parser)]
#[command(name = "mtseekd", version, max_term_width = 100)]
struct Args {
    /// Run in the foreground as the current user (no admin needed). Logs to stderr.
    #[arg(long, conflicts_with_all = ["status", "stop"])]
    console: bool,

    /// Show what the running daemon holds.
    #[arg(long, conflicts_with = "stop")]
    status: bool,

    /// Ask the running daemon to write what it holds in memory and stop.
    #[arg(long)]
    stop: bool,

    #[arg(
        long,
        hide = true,
        help = r"The pipe [default: MTSEEK_PIPE, else this user's \\.\pipe\mtseek-<SID>]"
    )]
    pipe: Option<String>,

    #[arg(
        long,
        help = r"The cache directory [default: MTSEEK_CACHE_DIR, else %LOCALAPPDATA%\mtseek\cache]"
    )]
    cache: Option<PathBuf>,
}

fn main() -> ExitCode {
    let args = Args::parse();
    match run(&args) {
        Ok(code) => code,
        Err(err) => {
            let _ = writeln!(io::stderr(), "mtseekd: {err}");
            ExitCode::from(2)
        }
    }
}

#[cfg(windows)]
fn run(args: &Args) -> io::Result<ExitCode> {
    use mtseek_ipc::client::Client;

    let pipe = match &args.pipe {
        Some(pipe) => pipe.clone(),
        None => mtseek_ipc::client::default_pipe()?,
    };
    if args.status || args.stop {
        let sid = mtseek_ntfs::pipe::user_sid()?;
        let Some(mut client) =
            Client::connect(&pipe, Duration::from_millis(500), &sid).map_err(io::Error::other)?
        else {
            let _ = writeln!(io::stderr(), "mtseekd: no daemon is running on {pipe}");
            return Ok(ExitCode::from(1));
        };
        let mut out = io::stdout().lock();
        if args.stop {
            client.shutdown().map_err(io::Error::other)?;
            writeln!(
                out,
                "stopped {} (pid {})",
                client.info().server,
                client.info().pid
            )?;
        } else {
            let status = client.status().map_err(io::Error::other)?;
            writeln!(out, "{} (pid {}) on {pipe}", status.server, status.pid)?;
            if let Some(cache) = &status.cache {
                writeln!(out, "cache {}", cache.to_path_buf().display())?;
            }
            for index in &status.indexes {
                writeln!(
                    out,
                    "content {}: {} files, {:.1} MiB{}{}{}",
                    index.root.to_path_buf().display(),
                    index.files,
                    mib(index.file_bytes),
                    if index.ignore_files {
                        ""
                    } else {
                        ", no ignore files"
                    },
                    if index.hidden { ", hidden files" } else { "" },
                    if index.unsaved {
                        ", changes not yet written"
                    } else {
                        ""
                    },
                )?;
            }
            for names in &status.names {
                writeln!(
                    out,
                    "names {}: {} entries",
                    names.root.to_path_buf().display(),
                    names.entries
                )?;
            }
        }
        return Ok(ExitCode::SUCCESS);
    }
    if !args.console {
        let _ = writeln!(
            io::stderr(),
            "mtseekd: service mode comes with milestone M3c; run `mtseekd --console`"
        );
        return Ok(ExitCode::from(2));
    }
    let cache = args
        .cache
        .clone()
        .or_else(mtseek_engine::Engine::default_cache_dir);
    let mut config = mtseek_daemon::Config::new(pipe, cache);
    config.log = true;
    mtseek_daemon::Daemon::bind(config)?.run()?;
    Ok(ExitCode::SUCCESS)
}

#[cfg(not(windows))]
fn run(_: &Args) -> io::Result<ExitCode> {
    Err(io::Error::other("mtseekd runs on Windows only"))
}

#[allow(clippy::cast_precision_loss)] // a size for people to read
fn mib(bytes: u64) -> f64 {
    bytes as f64 / f64::from(1 << 20)
}
