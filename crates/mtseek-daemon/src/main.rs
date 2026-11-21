//! `mtseekd`: the mtseek daemon (ADR 0013, 0014).
//!
//! * `mtseekd --console` runs it in the foreground as the current user, until
//!   `mtseekd --stop` or Ctrl+C.
//! * `mtseekd --install` registers it as the `mtseek` service (`LocalSystem`, automatic
//!   start), which serves every interactive user of the machine; `mtseekd --uninstall`
//!   removes it. Both need an elevated administrator. The SCM starts it as
//!   `mtseekd --service`.
//!
//! While one runs, `mtseek` answers through it: indexes stay open and current, so a search
//! skips opening and catching up.

use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use clap::Parser;

/// Keeps mtseek's indexes open and current, and answers `mtseek` searches over a local
/// named pipe.
#[derive(Debug, Parser)]
#[command(name = "mtseekd", version, max_term_width = 100)]
#[allow(clippy::struct_excessive_bools)] // one flag per mode, as clap parses them
struct Args {
    /// Run in the foreground as the current user (no admin needed). Logs to stderr.
    #[arg(long, conflicts_with_all = ["status", "stop", "install", "uninstall", "service"])]
    console: bool,

    /// Show what the running daemon holds (this user's, else the service).
    #[arg(long, conflicts_with_all = ["stop", "install", "uninstall", "service"])]
    status: bool,

    /// Ask the running daemon to write what it holds in memory and stop (this user's,
    /// else the service, which needs an elevated administrator).
    #[arg(long, conflicts_with_all = ["install", "uninstall", "service"])]
    stop: bool,

    /// Register and start the mtseek service, which serves every user of this machine
    /// (needs an elevated administrator).
    #[arg(long, conflicts_with_all = ["uninstall", "service"])]
    install: bool,

    /// Stop and remove the mtseek service (needs an elevated administrator).
    #[arg(long, conflicts_with = "service")]
    uninstall: bool,

    /// Run as the service. The SCM starts mtseekd this way; it can't be run by hand.
    #[arg(long, hide = true)]
    service: bool,

    #[arg(
        long,
        hide = true,
        default_value = "mtseek",
        help = "The service's name"
    )]
    name: String,

    #[arg(
        long,
        hide = true,
        help = r"The pipe [default: MTSEEK_PIPE, else this user's \\.\pipe\mtseek-<SID>; \\.\pipe\mtseek for the service]"
    )]
    pipe: Option<String>,

    #[arg(
        long,
        help = r"The cache directory [default: MTSEEK_CACHE_DIR, else %LOCALAPPDATA%\mtseek\cache; %ProgramData%\mtseek\cache for the service]"
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
    if args.service {
        return service::run(args);
    }
    if args.install {
        return service::install(args);
    }
    if args.uninstall {
        mtseek_ntfs::service::uninstall(&args.name, Duration::from_secs(30))?;
        writeln!(io::stdout(), "removed the {} service", args.name)?;
        return Ok(ExitCode::SUCCESS);
    }
    if args.status || args.stop {
        return control(args);
    }
    if !args.console {
        let _ = writeln!(
            io::stderr(),
            "mtseekd: run `mtseekd --console` (as you), or `mtseekd --install` (as an \
             administrator) for the service"
        );
        return Ok(ExitCode::from(2));
    }
    let pipe = match &args.pipe {
        Some(pipe) => pipe.clone(),
        None => mtseek_ipc::client::default_pipe()?,
    };
    let cache = args
        .cache
        .clone()
        .or_else(mtseek_engine::Engine::default_cache_dir);
    let mut config = mtseek_daemon::Config::new(pipe, cache);
    config.log = true;
    mtseek_daemon::Daemon::bind(config)?.run()?;
    Ok(ExitCode::SUCCESS)
}

/// `--status` and `--stop`: this user's daemon, else the service.
#[cfg(windows)]
fn control(args: &Args) -> io::Result<ExitCode> {
    use mtseek_ipc::client::Client;
    use mtseek_ntfs::pipe;

    let mut candidates = Vec::new();
    if let Some(name) = &args.pipe {
        candidates.push((name.clone(), pipe::user_sid()?));
    } else {
        candidates.push((mtseek_ipc::client::default_pipe()?, pipe::user_sid()?));
        if std::env::var_os("MTSEEK_PIPE").is_none() {
            candidates.push((pipe::SERVICE_PIPE.to_owned(), pipe::LOCAL_SYSTEM.to_owned()));
        }
    }
    let mut found = None;
    for (name, sid) in &candidates {
        if let Ok(Some(client)) = Client::connect(name, Duration::from_millis(500), sid) {
            found = Some((name.clone(), client));
            break;
        }
    }
    let Some((name, mut client)) = found else {
        let _ = writeln!(io::stderr(), "mtseekd: no daemon is running");
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
        return Ok(ExitCode::SUCCESS);
    }
    let status = client.status().map_err(io::Error::other)?;
    writeln!(out, "{} (pid {}) on {name}", status.server, status.pid)?;
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
    Ok(ExitCode::SUCCESS)
}

#[cfg(windows)]
mod service {
    use std::fmt::Write as _;
    use std::io::{self, Write};
    use std::path::{Path, PathBuf};
    use std::process::ExitCode;
    use std::time::Duration;

    use super::Args;

    /// `%ProgramData%\mtseek`.
    fn machine_dir() -> PathBuf {
        std::env::var_os("ProgramData")
            .map_or_else(|| PathBuf::from(r"C:\ProgramData"), PathBuf::from)
            .join("mtseek")
    }

    /// Quotes `arg` for a command line if it needs it.
    fn quoted(arg: &str) -> String {
        if arg.contains([' ', '\t']) {
            format!("\"{arg}\"")
        } else {
            arg.to_owned()
        }
    }

    /// `--install`: registers and starts the service, running this executable.
    pub(super) fn install(args: &Args) -> io::Result<ExitCode> {
        let exe = std::env::current_exe()?;
        let mut command = format!("\"{}\" --service", exe.display());
        if args.name != "mtseek" {
            let _ = write!(command, " --name {}", quoted(&args.name));
        }
        if let Some(pipe) = &args.pipe {
            let _ = write!(command, " --pipe {}", quoted(pipe));
        }
        if let Some(cache) = &args.cache {
            let cache = std::path::absolute(cache)?;
            let _ = write!(command, " --cache {}", quoted(&cache.to_string_lossy()));
        }
        mtseek_ntfs::service::install(
            &args.name,
            "mtseek search",
            "Keeps mtseek's indexes current and answers searches for every user of this \
             machine, showing each only what their account may read.",
            &command,
            true,
        )?;
        writeln!(
            io::stdout(),
            "installed and started the {} service",
            args.name
        )?;
        Ok(ExitCode::SUCCESS)
    }

    /// `--service`: what the SCM runs.
    pub(super) fn run(args: &Args) -> io::Result<ExitCode> {
        let pipe = args
            .pipe
            .clone()
            .unwrap_or_else(|| mtseek_ntfs::pipe::SERVICE_PIPE.to_owned());
        let (cache, logs) = match &args.cache {
            Some(cache) => (cache.clone(), cache.join("logs")),
            None => (machine_dir().join("cache"), machine_dir().join("logs")),
        };
        let secured = match &args.cache {
            Some(cache) => cache.clone(),
            None => machine_dir(),
        };
        mtseek_ntfs::service::run(move |service| {
            prepare(&secured, &cache, &logs).map_err(|e| e.to_string())?;
            let mut config = mtseek_daemon::Config::new(pipe.clone(), Some(cache.clone()));
            config.mode = mtseek_daemon::Mode::Service;
            config.log_file = Some(logs.join("mtseekd.log"));
            // Another process may hold the pipe's name (squatting, or an old instance
            // stopping): try again every 5 s rather than give up.
            let running = loop {
                match mtseek_daemon::Daemon::spawn(config.clone()) {
                    Ok(running) => break running,
                    Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                        if service.wait_for_stop_timeout(Duration::from_secs(5)) {
                            return Ok(());
                        }
                    }
                    Err(e) => return Err(e.to_string()),
                }
            };
            service.running();
            service.wait_for_stop();
            running.stop().map_err(|e| e.to_string())
        })?;
        Ok(ExitCode::SUCCESS)
    }

    /// The machine folder, locked to SYSTEM and Administrators, with the cache and log
    /// folders inside (ADR 0014 §2).
    fn prepare(secured: &Path, cache: &Path, logs: &Path) -> io::Result<()> {
        mtseek_ntfs::service::secure_dir(secured)?;
        std::fs::create_dir_all(cache)?;
        std::fs::create_dir_all(logs)
    }
}

#[cfg(not(windows))]
fn run(_: &Args) -> io::Result<ExitCode> {
    Err(io::Error::other("mtseekd runs on Windows only"))
}

#[allow(clippy::cast_precision_loss)] // a size for people to read
fn mib(bytes: u64) -> f64 {
    bytes as f64 / f64::from(1 << 20)
}
