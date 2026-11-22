//! Answering through the daemon (ADR 0013, 0014).
//!
//! The CLI tries the user's daemon pipe, then the service's, each with a short timeout. No
//! daemon, a daemon that speaks another protocol version, or one running as someone it
//! shouldn't (another user; for the service, anyone but `LocalSystem`): the CLI searches in
//! this process instead, silently (`--daemon` makes that an error). The daemon gets the
//! search exactly as parsed, plus this process's working directory, which folder each root
//! is and git's global excludes file, and sends back what the direct engine would have
//! printed. The service may decline a search it can't answer exactly; the CLI then
//! searches in this process, and `--stats` says why.

use std::time::{Duration, Instant};

use mtseek_engine::{NamesRequest, Outcome, SearchRequest};

use crate::{Args, Failure, Printer};

/// How long to wait for a busy daemon before searching directly.
#[cfg_attr(not(windows), allow(dead_code))]
const CONNECT_TIMEOUT: Duration = Duration::from_millis(50);

#[cfg(windows)]
pub(crate) use imp::Client;

/// What the daemon did with a search.
pub(crate) enum Answer {
    /// Answered; `None` if the printer stopped it (stdout closed).
    Done(Option<Outcome>),
    /// Left to this process, for this reason (ADR 0014 §6). Nothing was printed.
    Declined(String),
}

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

/// Runs a content search on the daemon, printing as results arrive.
#[cfg(windows)]
pub(crate) fn search(
    client: &mut Client,
    request: &SearchRequest,
    printer: &Printer,
) -> Result<Answer, Failure> {
    imp::search(client, request, printer)
}

#[cfg(not(windows))]
pub(crate) fn search(_: &mut Client, _: &SearchRequest, _: &Printer) -> Result<Answer, Failure> {
    unreachable!("no client without Windows")
}

/// Runs a name search on the daemon, printing paths as they arrive. Returns how many
/// matched and the `--stats` lines, or why the daemon declined.
#[cfg(windows)]
pub(crate) fn names(
    client: &mut Client,
    request: &NamesRequest,
) -> Result<Result<(u64, Vec<String>), String>, Failure> {
    imp::names(client, request)
}

#[cfg(not(windows))]
pub(crate) fn names(
    _: &mut Client,
    _: &NamesRequest,
) -> Result<Result<(u64, Vec<String>), String>, Failure> {
    unreachable!("no client without Windows")
}

/// The `--stats` line for a search the daemon left to this process.
#[cfg(windows)]
pub(crate) fn declined_by(client: &Client, why: &str) -> String {
    format!(
        "{} (pid {}) declined: {why}; answered in this process",
        client.info().server,
        client.info().pid
    )
}

#[cfg(not(windows))]
pub(crate) fn declined_by(_: &Client, _: &str) -> String {
    String::new()
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
    use mtseek_ipc::{
        Case, GlobalExcludes, Hit, IndexMode, NamesParams, RootId, SearchMode, SearchParams,
        WirePath, code,
    };
    use mtseek_walk::{FileHit, LineHit, Mode, Summary};

    use super::{Answer, CONNECT_TIMEOUT};
    use crate::{Failure, PathWriter, Printer, hint};

    /// The global excludes file is sent whole up to this size; the service only needs to
    /// know whether it holds rules.
    const MAX_EXCLUDES: usize = 1 << 20;

    /// This user's daemon, else the service.
    pub(super) fn connect() -> Result<Option<Client>, Failure> {
        let pipe = mtseek_ipc::client::default_pipe()?;
        let sid = mtseek_ntfs::pipe::user_sid()?;
        let mine = Client::connect(&pipe, CONNECT_TIMEOUT, &sid);
        if let Ok(Some(client)) = mine {
            return Ok(Some(client));
        }
        let Some(service) = mtseek_ipc::client::service_pipe() else {
            return Ok(mine?);
        };
        match Client::connect(&service, CONNECT_TIMEOUT, mtseek_ntfs::pipe::LOCAL_SYSTEM) {
            Ok(Some(client)) => Ok(Some(client)),
            // Why the service can't be used says more than "no daemon" (with `--daemon`).
            Err(err) if matches!(mine, Ok(None)) => Err(err.into()),
            Ok(None) | Err(_) => Ok(mine?),
        }
    }

    /// Which folder each root is, for the service to check (ADR 0014 §5).
    fn root_ids(roots: &[PathBuf], cwd: &std::path::Path) -> Vec<Option<RootId>> {
        roots
            .iter()
            .map(|root| {
                mtseek_ntfs::identity(&cwd.join(root))
                    .ok()
                    .map(|(volume, frn)| RootId { volume, frn })
            })
            .collect()
    }

    /// Git's global excludes file as this process's `rg` would read it.
    fn global_excludes() -> GlobalExcludes {
        match mtseek_walk::global_excludes() {
            Some((path, mut bytes)) => {
                bytes.truncate(MAX_EXCLUDES);
                GlobalExcludes {
                    path: Some(WirePath::new(&path)),
                    b64: mtseek_ipc::base64::encode(&bytes),
                }
            }
            None => GlobalExcludes {
                path: None,
                b64: String::new(),
            },
        }
    }

    pub(super) fn search(
        client: &mut Client,
        request: &SearchRequest,
        printer: &Printer,
    ) -> Result<Answer, Failure> {
        let cwd = std::env::current_dir()?;
        let params = SearchParams {
            pattern: request.query.pattern.clone(),
            fixed: request.query.kind == PatternKind::Literal,
            case: case(request.query.case),
            roots: request.roots.iter().map(|r| WirePath::new(r)).collect(),
            cwd: WirePath::new(&cwd),
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
            root_ids: Some(root_ids(&request.roots, &cwd)),
            global_excludes: Some(global_excludes()),
        };
        let mut forward = Forward {
            printer,
            paths: None,
            bad: None,
        };
        let done = match client.search(&params, &mut forward) {
            Err(ClientError::Server(err)) if err.code == code::DECLINED => {
                return Ok(Answer::Declined(err.message));
            }
            done => done.map_err(failure)?,
        };
        if let Some(bad) = forward.bad {
            return Err(bad.into());
        }
        Ok(Answer::Done(done.map(|result| Outcome {
            summary: Summary {
                files_searched: result.summary.searched,
                files_matched: result.summary.matched,
                errors: result.summary.errors,
            },
            stats: result.stats,
        })))
    }

    pub(super) fn names(
        client: &mut Client,
        request: &NamesRequest,
    ) -> Result<Result<(u64, Vec<String>), String>, Failure> {
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
        let done = match done {
            Err(ClientError::Server(err)) if err.code == code::DECLINED => {
                return Ok(Err(err.message));
            }
            done => done.map_err(failure)?,
        };
        Ok(Ok(done.map_or((1, Vec::new()), |r| (r.matched, r.stats))))
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
