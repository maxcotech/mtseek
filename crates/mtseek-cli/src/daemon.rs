//! Answering through the daemon (ADR 0013).
//!
//! The CLI tries the user's daemon pipe with a short timeout. No daemon, a daemon that
//! speaks another protocol version, or one running as another user: the CLI searches in
//! this process instead, silently (`--daemon` makes that an error). The daemon gets the
//! search exactly as parsed, plus this process's working directory, and sends back what the
//! direct engine would have printed.

use std::time::{Duration, Instant};

use mtseek_engine::{NamesRequest, Outcome, SearchRequest};

use crate::{Args, Failure, Printer};

/// How long to wait for a busy daemon before searching directly.
#[cfg_attr(not(windows), allow(dead_code))]
const CONNECT_TIMEOUT: Duration = Duration::from_millis(50);

#[cfg(windows)]
pub(crate) use imp::Client;

/// The daemon to answer this run, or `None` to search in this process.
#[cfg(windows)]
pub(crate) fn connect(args: &Args) -> Result<Option<Client>, Failure> {
    if args.no_daemon {
        return Ok(None);
    }
    let found = imp::connect();
    match found {
        Ok(Some(client)) => Ok(Some(client)),
        Ok(None) if args.daemon => Err("no daemon (mtseekd) is running".into()),
        Err(err) if args.daemon => Err(err),
        Ok(None) | Err(_) => Ok(None),
    }
}

#[cfg(not(windows))]
pub(crate) struct Client;

#[cfg(not(windows))]
pub(crate) fn connect(args: &Args) -> Result<Option<Client>, Failure> {
    if args.daemon {
        return Err("the daemon runs on Windows only".into());
    }
    Ok(None)
}

/// Runs a content search on the daemon, printing as results arrive. `Ok(None)` if the
/// printer stopped it (stdout closed).
#[cfg(windows)]
pub(crate) fn search(
    client: &mut Client,
    request: &SearchRequest,
    printer: &Printer,
) -> Result<Option<Outcome>, Failure> {
    imp::search(client, request, printer)
}

#[cfg(not(windows))]
pub(crate) fn search(
    _: &mut Client,
    _: &SearchRequest,
    _: &Printer,
) -> Result<Option<Outcome>, Failure> {
    unreachable!("no client without Windows")
}

/// Runs a name search on the daemon, printing paths as they arrive. Returns how many
/// matched and the `--stats` lines.
#[cfg(windows)]
pub(crate) fn names(
    client: &mut Client,
    request: &NamesRequest,
) -> Result<(u64, Vec<String>), Failure> {
    imp::names(client, request)
}

#[cfg(not(windows))]
pub(crate) fn names(_: &mut Client, _: &NamesRequest) -> Result<(u64, Vec<String>), Failure> {
    unreachable!("no client without Windows")
}

/// The `--stats` line saying who answered.
#[cfg(windows)]
pub(crate) fn answered_by(client: &Client, started: Instant) -> String {
    format!(
        "answered by {} (pid {}) on {} in {:.1} ms",
        client.info().server,
        client.info().pid,
        client.pipe(),
        started.elapsed().as_secs_f64() * 1000.0
    )
}

#[cfg(not(windows))]
pub(crate) fn answered_by(_: &Client, _: Instant) -> String {
    String::new()
}

#[cfg(windows)]
mod imp {
    use std::io::{self, Write};
    use std::ops::ControlFlow;
    use std::path::PathBuf;

    use mtseek_core::{CaseMode, PatternKind};
    use mtseek_engine::{IndexUse, NamesRequest, Outcome, Report, SearchRequest};
    pub(crate) use mtseek_ipc::client::Client;
    use mtseek_ipc::client::{ClientError, Sink};
    use mtseek_ipc::{Case, Hit, IndexMode, NamesParams, SearchMode, SearchParams, WirePath};
    use mtseek_walk::{FileHit, LineHit, Mode, Summary};

    use super::CONNECT_TIMEOUT;
    use crate::{Failure, PathWriter, Printer, hint};

    pub(super) fn connect() -> Result<Option<Client>, Failure> {
        let pipe = mtseek_ipc::client::default_pipe()?;
        let sid = mtseek_ntfs::pipe::user_sid()?;
        Ok(Client::connect(&pipe, CONNECT_TIMEOUT, &sid)?)
    }

    pub(super) fn search(
        client: &mut Client,
        request: &SearchRequest,
        printer: &Printer,
    ) -> Result<Option<Outcome>, Failure> {
        let params = SearchParams {
            pattern: request.query.pattern.clone(),
            fixed: request.query.kind == PatternKind::Literal,
            case: case(request.query.case),
            roots: request.roots.iter().map(|r| WirePath::new(r)).collect(),
            cwd: WirePath::new(&std::env::current_dir()?),
            ignore_files: request.filter.respect_ignore_files,
            hidden: request.filter.include_hidden,
            binary: request.filter.include_binary,
            follow: request.filter.follow_links,
            mode: match request.mode {
                Mode::Files => SearchMode::Files,
                Mode::Lines => SearchMode::Lines,
            },
            threads: request.threads,
            index: match request.index {
                IndexUse::Auto => IndexMode::Auto,
                IndexUse::Build => IndexMode::Build,
                IndexUse::Rebuild => IndexMode::Rebuild,
                IndexUse::Off => IndexMode::Off,
            },
        };
        let mut forward = Forward {
            printer,
            paths: None,
            bad: None,
        };
        let done = client.search(&params, &mut forward).map_err(failure)?;
        if let Some(bad) = forward.bad {
            return Err(bad.into());
        }
        Ok(done.map(|result| Outcome {
            summary: Summary {
                files_searched: result.summary.searched,
                files_matched: result.summary.matched,
                errors: result.summary.errors,
            },
            stats: result.stats,
        }))
    }

    pub(super) fn names(
        client: &mut Client,
        request: &NamesRequest,
    ) -> Result<(u64, Vec<String>), Failure> {
        let params = NamesParams {
            pattern: request.pattern.clone(),
            case: case(request.case),
            roots: request.roots.iter().map(|r| WirePath::new(r)).collect(),
            cwd: WirePath::new(&std::env::current_dir()?),
            threads: request.threads,
        };
        let printer = Printer::new(Mode::Files, crate::Format::default());
        let mut forward = Forward {
            printer: &printer,
            paths: Some(PathWriter::new()),
            bad: None,
        };
        let done = client.names(&params, &mut forward);
        if let Some(out) = forward.paths.take() {
            out.finish()?;
        }
        let done = done.map_err(failure)?;
        Ok(done.map_or((1, Vec::new()), |r| (r.matched, r.stats)))
    }

    /// A failed search prints the daemon's message, exactly as direct mode prints its error.
    fn failure(err: ClientError) -> Failure {
        match err {
            ClientError::Server(err) => err.message.into(),
            other => Box::new(other),
        }
    }

    fn case(case: CaseMode) -> Case {
        match case {
            CaseMode::Sensitive => Case::Sensitive,
            CaseMode::Insensitive => Case::Insensitive,
            CaseMode::Smart => Case::Smart,
        }
    }

    /// Prints what the daemon sends.
    struct Forward<'a> {
        printer: &'a Printer,
        paths: Option<PathWriter>,
        bad: Option<String>,
    }

    impl Sink for Forward<'_> {
        fn hit(&mut self, hit: Hit) -> ControlFlow<()> {
            let mut lines = Vec::with_capacity(hit.lines.len());
            for line in &hit.lines {
                let Some(bytes) = line.bytes() else {
                    self.bad = Some("the daemon sent a malformed line".into());
                    return ControlFlow::Break(());
                };
                lines.push(LineHit {
                    line_number: line.n,
                    bytes,
                });
            }
            let path: PathBuf = hit.path.to_path_buf();
            self.printer.hit(&FileHit {
                path: &path,
                lines: &lines,
                binary_offset: hit.binary_offset,
            })
        }

        fn paths(&mut self, paths: Vec<WirePath>) -> ControlFlow<()> {
            let Some(out) = &mut self.paths else {
                return ControlFlow::Continue(());
            };
            for path in paths {
                if !out.write(&path.to_path_buf()) {
                    return ControlFlow::Break(());
                }
            }
            ControlFlow::Continue(())
        }

        fn error(&mut self, message: String) {
            let _ = writeln!(io::stderr(), "mtseek: {message}");
        }

        fn note(&mut self, message: String) {
            hint(&message);
        }
    }
}
