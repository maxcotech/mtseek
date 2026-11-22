//! The daemon (ADR 0013): one resident [`Engine`] behind a local named pipe.
//!
//! * Each connection gets a thread. It reads one request at a time and answers it; a search
//!   streams `$/hit` notifications as files match, then its result.
//! * Searches run one at a time on the shared engine, whose indexes stay open.
//! * A poller thread catches the open indexes up from the change journal every
//!   [`Config::poll`], so a search usually finds them current, and writes deltas to disk
//!   once they have been quiet for [`Config::quiet`]. Writing never adds to a search.
//! * A search stops when its client sends `$/cancel` or goes away.
//! * Every search runs for its client's account (ADR 0014): the client's token, taken from
//!   the pipe, decides what it may see.
//!
//! Two modes ([`Mode`]). A console daemon serves its own user. The service serves every
//! interactive user of the machine from indexes SYSTEM built: it answers only what an index
//! answers exactly for the caller and declines the rest (`DECLINED`), keeps paths and queries
//! out of its log, shows each caller only the coverage it may list, and stops only for an
//! elevated administrator.
//!
//! The binary is `mtseekd`. Tests run the daemon in-process with [`Daemon::spawn`].

#![cfg(windows)]

use std::collections::VecDeque;
use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use mtseek_core::{CaseMode, Error, Filter, PatternKind, Query, Select, TypeRule};
use mtseek_engine::{Caller, Engine, IndexUse, NamesRequest, Outcome, Report, SearchRequest};
use mtseek_ipc::{
    CancelParams, Case, Hit, Incoming, IndexMode, IndexStatus, InitializeParams, InitializeResult,
    Line, MIN_PROTOCOL, Message, NamesParams, NamesResult, NamesStatus, PROTOCOL, Paths, Reader,
    RpcError, SearchMode, SearchParams, SearchResult, StatusResult, Summary, WirePath, code,
};
use mtseek_ntfs::pipe::{self, Listener};
use mtseek_ntfs::security::{self, Token};
use mtseek_query::Viewer;
use mtseek_walk::{FileHit, Mode as WalkMode, Output};
use serde_json::Value;

/// How the daemon runs.
#[derive(Debug, Clone)]
pub struct Config {
    /// The pipe to listen on.
    pub pipe: String,
    /// The cache directory (indexes and the coverage list).
    pub cache: Option<PathBuf>,
    /// How often the poller catches open indexes up.
    pub poll: Duration,
    /// How long changes must be quiet before they are written.
    pub quiet: Duration,
    /// Log requests and writes to stderr.
    pub log: bool,
    /// Also log to this file, rotated at [`LOG_LIMIT`] (the service).
    pub log_file: Option<PathBuf>,
    /// Console (one user) or service (every user of the machine).
    pub mode: Mode,
}

/// Who a daemon serves (ADR 0013, 0014).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mode {
    /// The user it runs as, on that user's pipe.
    #[default]
    Console,
    /// Every interactive user, on [`pipe::SERVICE_PIPE`], running as `LocalSystem`.
    Service,
}

/// A log file grows to this size, then is renamed to `.1` and started again.
pub const LOG_LIMIT: u64 = 8 << 20;

impl Config {
    /// The defaults of ADR 0013: poll every 250 ms, write after 2 s without changes.
    pub fn new(pipe: impl Into<String>, cache: Option<PathBuf>) -> Self {
        Self {
            pipe: pipe.into(),
            cache,
            poll: Duration::from_millis(250),
            quiet: Duration::from_secs(2),
            log: false,
            log_file: None,
            mode: Mode::Console,
        }
    }
}

/// The methods this daemon answers, for `initialize`.
const METHODS: &[&str] = &["search", "names", "status", "shutdown", "$/cancel"];

/// How many names go in one `$/paths` notification.
const PATHS_PER_MESSAGE: usize = 1000;

/// How often (in milliseconds) a running search looks for `$/cancel`.
const CANCEL_CHECK_MS: u64 = 10;

struct Shared {
    config: Config,
    engine: Mutex<Engine>,
    stop: AtomicBool,
    log_file: Mutex<Option<File>>,
}

impl Shared {
    fn engine(&self) -> MutexGuard<'_, Engine> {
        self.engine.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn service(&self) -> bool {
        self.config.mode == Mode::Service
    }

    /// Logs a line that names no path and no query.
    fn log(&self, line: &str) {
        if self.config.log {
            let _ = writeln!(io::stderr(), "mtseekd: {line}");
        }
        let Some(path) = &self.config.log_file else {
            return;
        };
        let mut file = lock(&self.log_file);
        if file
            .as_ref()
            .and_then(|f| f.metadata().ok())
            .is_some_and(|m| m.len() > LOG_LIMIT)
        {
            *file = None;
            let _ = std::fs::rename(path, path.with_extension("log.1"));
        }
        if file.is_none() {
            *file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .ok();
        }
        if let Some(f) = file.as_mut() {
            let secs = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_secs());
            let _ = writeln!(f, "{secs} {line}");
        }
    }

    /// Logs what saving wrote: each line in console mode, only how many in the service,
    /// whose lines would name the folders users index.
    fn log_saved(&self, lines: &[String]) {
        if self.service() {
            if !lines.is_empty() {
                self.log(&format!(
                    "{} write(s) of changes kept in memory",
                    lines.len()
                ));
            }
        } else {
            for line in lines {
                self.log(line);
            }
        }
    }
}

/// A daemon bound to its pipe, not yet serving.
pub struct Daemon {
    listener: Listener,
    shared: Arc<Shared>,
}

/// A daemon serving on a thread of this process (tests, and the service host).
pub struct Running {
    pipe: String,
    shared: Arc<Shared>,
    thread: Option<JoinHandle<io::Result<()>>>,
}

impl Running {
    /// The pipe it listens on.
    pub fn pipe(&self) -> &str {
        &self.pipe
    }

    /// Writes what it holds in memory, stops it, and waits until it has.
    pub fn stop(mut self) -> io::Result<()> {
        self.shutdown()
    }

    fn shutdown(&mut self) -> io::Result<()> {
        let Some(thread) = self.thread.take() else {
            return Ok(());
        };
        {
            let mut engine = self.shared.engine();
            let saved = engine.save(Duration::ZERO);
            self.shared.log_saved(&saved);
            self.shared.stop.store(true, Ordering::SeqCst);
        }
        self.shared.log("stopping");
        // Wake the accept loop, which then sees the flag.
        let _ = pipe::connect(&self.pipe, Duration::from_secs(1), &pipe::user_sid()?);
        thread
            .join()
            .unwrap_or_else(|_| Err(io::Error::other("the daemon thread panicked")))
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}

impl Daemon {
    /// Creates the pipe. Fails if another daemon holds it.
    pub fn bind(config: Config) -> io::Result<Self> {
        let listener = match config.mode {
            Mode::Console => Listener::bind(&config.pipe)?,
            Mode::Service => Listener::bind_with(&config.pipe, pipe::SERVICE_PIPE_SDDL)?,
        };
        let engine = Engine::resident(config.cache.clone());
        Ok(Self {
            listener,
            shared: Arc::new(Shared {
                config,
                engine: Mutex::new(engine),
                stop: AtomicBool::new(false),
                log_file: Mutex::new(None),
            }),
        })
    }

    /// Binds and serves on a new thread.
    pub fn spawn(config: Config) -> io::Result<Running> {
        let pipe = config.pipe.clone();
        let daemon = Self::bind(config)?;
        let shared = Arc::clone(&daemon.shared);
        let thread = std::thread::Builder::new()
            .name("mtseekd".into())
            .spawn(move || daemon.run())?;
        Ok(Running {
            pipe,
            shared,
            thread: Some(thread),
        })
    }

    /// Serves until a client sends `shutdown`.
    pub fn run(mut self) -> io::Result<()> {
        let shared = Arc::clone(&self.shared);
        shared.log(&format!(
            "listening on {} (cache {})",
            self.listener.name(),
            shared
                .config
                .cache
                .as_ref()
                .map_or_else(|| "none".into(), |c| c.display().to_string())
        ));
        let poller = {
            let shared = Arc::clone(&shared);
            std::thread::Builder::new()
                .name("mtseekd-poll".into())
                .spawn(move || poll(&shared))?
        };
        let served = loop {
            let conn = match self.listener.accept() {
                Ok(conn) => conn,
                Err(err) => break Err(err),
            };
            if shared.stop.load(Ordering::SeqCst) {
                break Ok(());
            }
            let shared = Arc::clone(&shared);
            let spawned = std::thread::Builder::new()
                .name("mtseekd-conn".into())
                .spawn(move || serve(conn, &shared));
            if let Err(err) = spawned {
                break Err(err);
            }
        };
        shared.stop.store(true, Ordering::SeqCst);
        let _ = poller.join();
        served
    }
}

/// The poller: catches open indexes up, and writes what has been quiet long enough.
fn poll(shared: &Shared) {
    let step = shared.config.poll.max(Duration::from_millis(10));
    while !shared.stop.load(Ordering::SeqCst) {
        std::thread::sleep(step);
        let mut engine = shared.engine();
        if shared.stop.load(Ordering::SeqCst) {
            break;
        }
        let changed = engine.refresh();
        if changed > 0 {
            shared.log(&format!("{changed} index(es) caught up"));
        }
        let saved = engine.save(shared.config.quiet);
        shared.log_saved(&saved);
    }
}

/// One connection: requests in, answers out.
fn serve(conn: File, shared: &Shared) {
    let Ok(reading) = conn.try_clone() else {
        return;
    };
    let mut session = Session {
        shared,
        out: Mutex::new(BufWriter::with_capacity(64 * 1024, conn)),
        inbox: Inbox {
            reader: Mutex::new(Reader::new(reading)),
            pending: Mutex::new(VecDeque::new()),
        },
        initialized: false,
        caller: None,
    };
    loop {
        let line = match session.inbox.next() {
            Ok(Some(line)) => line,
            Ok(None) => break,
            Err(err) => {
                shared.log(&format!("closing a connection: {err}"));
                break;
            }
        };
        match session.handle(&line) {
            Ok(Flow::Next) => {}
            // Stopping, or the client went away.
            Ok(Flow::Stop) | Err(_) => break,
        }
    }
}

/// Lines from the client, with those read during a search kept for afterwards.
struct Inbox {
    reader: Mutex<Reader<File>>,
    pending: Mutex<VecDeque<Vec<u8>>>,
}

impl Inbox {
    fn next(&self) -> io::Result<Option<Vec<u8>>> {
        if let Some(line) = lock(&self.pending).pop_front() {
            return Ok(Some(line));
        }
        Ok(lock(&self.reader).read_line()?.map(<[u8]>::to_vec))
    }

    /// Reads what the client sent while a search runs, without blocking: whether it asks
    /// to cancel request `id`, or is gone. Other messages wait in `pending`.
    fn cancelled(&self, id: &Value) -> bool {
        let Ok(mut reader) = self.reader.try_lock() else {
            return false;
        };
        loop {
            if !reader.buffered() {
                match pipe::available(reader.get_ref()) {
                    Ok(0) => return false,
                    Ok(_) => {}
                    Err(_) => return true,
                }
            }
            let line = match reader.read_line() {
                Ok(Some(line)) => line.to_vec(),
                Ok(None) | Err(_) => return true,
            };
            if let Ok(message) = Incoming::parse(&line)
                && message.method.as_deref() == Some("$/cancel")
            {
                if message.params::<CancelParams>().is_ok_and(|c| &c.id == id) {
                    return true;
                }
                continue;
            }
            lock(&self.pending).push_back(line);
        }
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

enum Flow {
    Next,
    Stop,
}

struct Session<'a> {
    shared: &'a Shared,
    out: Mutex<BufWriter<File>>,
    inbox: Inbox,
    initialized: bool,
    /// Who the client is, from its token (set by `initialize`).
    caller: Option<Caller>,
}

impl Session<'_> {
    fn send(&self, bytes: &[u8]) -> io::Result<()> {
        let mut out = lock(&self.out);
        out.write_all(bytes)?;
        out.flush()
    }

    fn reply(&self, id: &Value, result: Result<Value, RpcError>) -> io::Result<()> {
        let mut bytes = Vec::new();
        match result {
            Ok(value) => mtseek_ipc::result(&mut bytes, id, &value),
            Err(err) => mtseek_ipc::error(&mut bytes, id, &err),
        }
        self.send(&bytes)
    }

    fn handle(&mut self, line: &[u8]) -> io::Result<Flow> {
        let message = match Incoming::parse(line) {
            Ok(m) => m,
            Err(e) => {
                self.reply(
                    &Value::Null,
                    Err(rpc(code::PARSE, format!("not JSON: {e}"))),
                )?;
                return Ok(Flow::Next);
            }
        };
        let Some(method) = message.method.as_deref() else {
            return Ok(Flow::Next); // a response: this server sends no requests
        };
        let Some(id) = message.id.clone() else {
            return Ok(Flow::Next); // a notification: `$/cancel` of a finished search
        };
        if !self.initialized && method != "initialize" {
            let err = rpc(code::NOT_INITIALIZED, "send initialize first".into());
            self.reply(&id, Err(err))?;
            return Ok(Flow::Next);
        }
        let started = Instant::now();
        let mut declined = None;
        let result = match method {
            "initialize" => self.initialize(&message),
            "search" => match message.params::<SearchParams>() {
                Ok(params) => self.search(&id, &params),
                Err(e) => Err(rpc(code::INVALID_PARAMS, e.to_string())),
            },
            "names" if self.shared.service() => Err(rpc(
                code::DECLINED,
                "the service doesn't answer name searches yet".into(),
            )),
            "names" => match message.params::<NamesParams>() {
                Ok(params) => self.names(&id, &params),
                Err(e) => Err(rpc(code::INVALID_PARAMS, e.to_string())),
            },
            "status" => Ok(self.status()),
            "shutdown" if self.shared.service() && !self.caller().is_admin() => Err(rpc(
                code::FAILED,
                "only an elevated administrator can stop the service (or `sc stop mtseek`)".into(),
            )),
            "shutdown" => {
                self.shutdown(&id)?;
                return Ok(Flow::Stop);
            }
            _ => Err(rpc(code::METHOD_NOT_FOUND, format!("no method {method:?}"))),
        };
        if let Err(err) = &result
            && err.code == code::DECLINED
        {
            declined = Some(err.message.clone());
        }
        let who = if self.shared.service() {
            self.caller
                .as_ref()
                .and_then(Caller::user_sid)
                .map(|sid| format!(" for {sid}"))
                .unwrap_or_default()
        } else {
            String::new()
        };
        let what = match (&declined, result.is_ok()) {
            (Some(why), _) => format!("declined ({why})"),
            (None, true) => "answered".into(),
            (None, false) => "failed".into(),
        };
        self.shared.log(&format!(
            "{method}{who} {what} in {:.1} ms",
            started.elapsed().as_secs_f64() * 1000.0
        ));
        self.reply(&id, result)?;
        Ok(Flow::Next)
    }

    fn initialize(&mut self, message: &Incoming<'_>) -> Result<Value, RpcError> {
        let params: InitializeParams = message
            .params()
            .map_err(|e| rpc(code::INVALID_PARAMS, e.to_string()))?;
        let Some(protocol) = mtseek_ipc::negotiate([MIN_PROTOCOL, PROTOCOL], params.protocol)
        else {
            return Err(rpc(
                code::UNSUPPORTED_VERSION,
                format!(
                    "this daemon speaks protocol {MIN_PROTOCOL}..={PROTOCOL}, the client {:?}",
                    params.protocol
                ),
            ));
        };
        // Who the client is: Windows identifies it once it has written, which it has.
        let token = Token::of_client(lock(&self.out).get_ref());
        self.caller = match (token, self.shared.config.mode) {
            (Ok(token), Mode::Service) => Some(Caller::client(token, true)),
            (Ok(token), Mode::Console) => Some(Caller::client(token, false)),
            (Err(_), Mode::Console) => Some(Caller::process()),
            (Err(e), Mode::Service) => {
                return Err(rpc(code::FAILED, format!("can't identify the client: {e}")));
            }
        };
        self.initialized = true;
        self.shared.log(&format!("{} connected", params.client));
        to_value(&InitializeResult {
            server: server_name(),
            protocol,
            pid: std::process::id(),
            methods: METHODS.iter().map(|&m| m.to_owned()).collect(),
        })
    }

    /// The client, as identified by `initialize`.
    fn caller(&self) -> &Caller {
        self.caller
            .as_ref()
            .expect("requests come after initialize, which sets the caller")
    }

    fn search(&self, id: &Value, params: &SearchParams) -> Result<Value, RpcError> {
        let request = search_request(params);
        let report = PipeReport::new(id, self);
        let outcome = if self.shared.service() {
            self.serve(params, &request, &report)
        } else {
            self.shared
                .engine()
                .search_as(&request, self.caller(), &report)
        };
        report.finish()?;
        let outcome = outcome.map_err(|e| match e {
            Error::Declined(why) => rpc(code::DECLINED, why),
            e => rpc(code::FAILED, e.to_string()),
        })?;
        to_value(&SearchResult {
            summary: Summary {
                searched: outcome.summary.files_searched,
                matched: outcome.summary.files_matched,
                errors: outcome.summary.errors,
                bytes: outcome.summary.bytes_searched,
                nanos: nanos(outcome.summary.search_time),
            },
            stats: outcome.stats,
        })
    }

    /// A search in the service (ADR 0014 §5, §6): only what an index answers exactly for
    /// this caller. Everything is checked before the engine runs, which itself declines
    /// before reporting anything.
    fn serve(
        &self,
        params: &SearchParams,
        request: &SearchRequest,
        report: &PipeReport<'_>,
    ) -> mtseek_core::Result<Outcome> {
        let decline = |why: &str| Err(Error::Declined(why.to_owned()));
        let caller = self.caller();
        let Some(ids) = &params.root_ids else {
            return decline("the client didn't say which folders its roots are");
        };
        let Some(excludes) = &params.global_excludes else {
            return decline("the client didn't send its global excludes file");
        };
        let excludes = mtseek_ipc::base64::decode(&excludes.b64).unwrap_or_default();
        if request.filter.respect_ignore_files && mtseek_walk::has_ignore_rules(&excludes) {
            return decline("the caller has a global git excludes file");
        }
        if ids.len() != request.roots.len() {
            return decline("the root identities don't match the roots");
        }
        let cwd = params.cwd.to_path_buf();
        let mut disk = Vec::with_capacity(request.roots.len());
        for (typed, id) in request.roots.iter().zip(ids) {
            let absolute = mtseek_engine::absolute_from(&cwd, typed);
            let Some(id) = id else {
                return decline("a root the caller can't open");
            };
            match mtseek_ntfs::identity(&absolute) {
                Ok((volume, frn)) if volume == id.volume && frn == id.frn => {}
                _ => return decline("a root names another folder here than for the caller"),
            }
            if request.index.explicit() {
                register_check(caller, typed, &absolute)?;
            }
            disk.push(absolute);
        }
        self.shared.engine().serve(request, &disk, caller, report)
    }

    fn names(&self, id: &Value, params: &NamesParams) -> Result<Value, RpcError> {
        let request = NamesRequest {
            pattern: params.pattern.clone(),
            case: case_mode(params.case),
            roots: params.roots.iter().map(WirePath::to_path_buf).collect(),
            threads: params.threads,
            cwd: Some(params.cwd.to_path_buf()),
        };
        let report = PipeReport::new(id, self);
        let found = self.shared.engine().names(&request, &report);
        report.finish()?;
        let found = found.map_err(|e| rpc(code::FAILED, e.to_string()))?;
        for batch in found.paths.chunks(PATHS_PER_MESSAGE) {
            let mut bytes = Vec::new();
            let paths = Paths {
                id: id.clone(),
                paths: batch.iter().map(|p| WirePath::new(p)).collect(),
            };
            mtseek_ipc::notification(&mut bytes, "$/paths", &paths);
            if self.send(&bytes).is_err() || self.inbox.cancelled(id) {
                return Err(rpc(code::CANCELLED, "cancelled".into()));
            }
        }
        to_value(&NamesResult {
            matched: found.paths.len() as u64,
            stats: found.stats,
        })
    }

    fn status(&self) -> Value {
        let engine = self.shared.engine();
        // The service shows each caller the coverage roots it may list; administrators all.
        let shown = |root: &Path| {
            !self.shared.service()
                || self.caller().is_admin()
                || security::descriptor_at(root).is_ok_and(|sd| self.caller().may_list(&sd))
        };
        let status = StatusResult {
            server: server_name(),
            pid: std::process::id(),
            cache: engine.cache().map(WirePath::new),
            indexes: engine
                .status()
                .into_iter()
                .filter(|s| shown(&s.root))
                .map(|s| IndexStatus {
                    root: WirePath::new(&s.root),
                    ignore_files: s.ignore_files,
                    hidden: s.hidden,
                    files: s.files,
                    file_bytes: s.file_bytes,
                    unsaved: s.unsaved,
                    checkpoints: s.checkpoints,
                })
                .collect(),
            names: engine
                .names_status()
                .into_iter()
                .map(|(root, entries)| NamesStatus {
                    root: WirePath::new(&root),
                    entries: entries as u64,
                })
                .collect(),
        };
        serde_json::to_value(status).unwrap_or(Value::Null)
    }

    /// Writes what is in memory, answers, and wakes the accept loop so it can stop.
    fn shutdown(&self, id: &Value) -> io::Result<()> {
        {
            let mut engine = self.shared.engine();
            let saved = engine.save(Duration::ZERO);
            self.shared.log_saved(&saved);
            self.shared.stop.store(true, Ordering::SeqCst);
        }
        self.shared.log("stopping");
        self.reply(id, Ok(Value::Null))?;
        // Wait until the client has read the answer before the process may exit.
        let _ = lock(&self.out).get_ref().sync_all();
        if let Ok(sid) = pipe::user_sid() {
            let _ = pipe::connect(&self.shared.config.pipe, Duration::from_secs(1), &sid);
        }
        Ok(())
    }
}

/// Sends a search's findings down the pipe as notifications, and notices when the client
/// cancels or goes away.
struct PipeReport<'a> {
    id: &'a Value,
    session: &'a Session<'a>,
    cancelled: AtomicBool,
    /// Milliseconds since `started` at the last look for `$/cancel`.
    checked: AtomicU64,
    started: Instant,
}

impl<'a> PipeReport<'a> {
    fn new(id: &'a Value, session: &'a Session<'a>) -> Self {
        Self {
            id,
            session,
            cancelled: AtomicBool::new(false),
            checked: AtomicU64::new(0),
            started: Instant::now(),
        }
    }

    fn send(&self, method: &str, params: &impl serde::Serialize) {
        let mut bytes = Vec::new();
        mtseek_ipc::notification(&mut bytes, method, params);
        let written = lock(&self.session.out).write_all(&bytes);
        if written.is_err() {
            self.cancelled.store(true, Ordering::SeqCst);
        }
    }

    fn check(&self) {
        let now = u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX);
        let last = self.checked.load(Ordering::Relaxed);
        if now.saturating_sub(last) < CANCEL_CHECK_MS
            || self
                .checked
                .compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed)
                .is_err()
        {
            return;
        }
        if self.session.inbox.cancelled(self.id) {
            self.cancelled.store(true, Ordering::SeqCst);
        }
    }

    /// Flushes the notifications. A search that was cancelled answers with an error.
    fn finish(&self) -> Result<(), RpcError> {
        let flushed = lock(&self.session.out).flush();
        if flushed.is_err() || self.cancelled.load(Ordering::SeqCst) {
            return Err(rpc(code::CANCELLED, "cancelled".into()));
        }
        Ok(())
    }
}

impl Report for PipeReport<'_> {
    fn hit(&self, hit: &FileHit<'_>) -> ControlFlow<()> {
        if self.cancelled.load(Ordering::Relaxed) {
            return ControlFlow::Break(());
        }
        let wire = Hit {
            id: self.id.clone(),
            path: WirePath::new(hit.path),
            lines: hit
                .lines
                .iter()
                .map(|l| Line {
                    off: l.offset,
                    ctx: l.context,
                    m: l.matches
                        .iter()
                        .map(|&(s, e)| [s as u64, e as u64])
                        .collect(),
                    ..Line::new(l.line_number, &l.bytes)
                })
                .collect(),
            binary_offset: hit.binary_offset,
            matched_lines: hit.stats.matched_lines,
            matches: hit.stats.matches,
            bytes: hit.stats.bytes_searched,
            nanos: nanos(hit.stats.elapsed),
        };
        self.send("$/hit", &wire);
        self.check();
        if self.cancelled.load(Ordering::Relaxed) {
            ControlFlow::Break(())
        } else {
            ControlFlow::Continue(())
        }
    }

    fn error(&self, err: &Error) {
        self.send(
            "$/error",
            &Message {
                id: self.id.clone(),
                message: err.to_string(),
            },
        );
    }

    fn note(&self, message: &str) {
        self.send(
            "$/note",
            &Message {
                id: self.id.clone(),
                message: message.to_owned(),
            },
        );
        // A note comes before slow work (a build): let the client show it now.
        let _ = lock(&self.session.out).flush();
    }
}

/// Whether `caller` may make `absolute` (typed as `typed`) a coverage root of the service
/// (ADR 0014 §7): only a folder it may list, and a whole drive only as an elevated
/// administrator.
fn register_check(caller: &Caller, typed: &Path, absolute: &Path) -> mtseek_core::Result<()> {
    let refuse = |why: &str| {
        Err(Error::Io {
            path: typed.to_path_buf(),
            source: io::Error::new(io::ErrorKind::PermissionDenied, why.to_owned()),
        })
    };
    let listable = security::descriptor_at(absolute).is_ok_and(|sd| caller.may_list(&sd));
    if !listable {
        return Err(Error::Declined(
            "the caller may not list a folder it asks to index".into(),
        ));
    }
    if absolute.parent().is_none() && !caller.is_admin() {
        return refuse("indexing a whole drive in the service needs an elevated administrator");
    }
    Ok(())
}

/// The engine request for `search` parameters.
pub fn search_request(params: &SearchParams) -> SearchRequest {
    let mut roots: Vec<PathBuf> = params.roots.iter().map(WirePath::to_path_buf).collect();
    if roots.is_empty() {
        roots.push(PathBuf::from("."));
    }
    SearchRequest {
        query: Query {
            pattern: params.pattern.clone(),
            kind: if params.fixed {
                PatternKind::Literal
            } else {
                PatternKind::Regex
            },
            case: case_mode(params.case),
        },
        roots,
        filter: Filter {
            respect_ignore_files: params.ignore_files,
            include_hidden: params.hidden,
            include_binary: params.binary,
            follow_links: params.follow,
        },
        select: Select {
            globs: params.globs.clone(),
            iglobs: params.iglobs.clone(),
            types: params
                .types
                .iter()
                .map(|t| TypeRule {
                    name: t.name.clone(),
                    negated: t.not,
                })
                .collect(),
            // Version 2 doesn't carry finer ignore settings: clients search those themselves.
            ignore_files: mtseek_core::IgnoreFiles::default(),
        },
        output: Output {
            mode: match params.mode {
                SearchMode::Files => WalkMode::Files,
                SearchMode::Lines => WalkMode::Lines,
                SearchMode::Count => WalkMode::Count,
                SearchMode::CountMatches => WalkMode::CountMatches,
            },
            before: params.before,
            after: params.after,
            max_count: params.max_count,
            positions: params.positions,
        },
        threads: params.threads,
        index: match params.index {
            IndexMode::Auto => IndexUse::Auto,
            IndexMode::Build => IndexUse::Build,
            IndexMode::Rebuild => IndexUse::Rebuild,
            IndexMode::Off => IndexUse::Off,
        },
        cwd: Some(params.cwd.to_path_buf()),
    }
}

fn nanos(d: Duration) -> u64 {
    u64::try_from(d.as_nanos()).unwrap_or(u64::MAX)
}

fn case_mode(case: Case) -> CaseMode {
    match case {
        Case::Sensitive => CaseMode::Sensitive,
        Case::Insensitive => CaseMode::Insensitive,
        Case::Smart => CaseMode::Smart,
    }
}

fn server_name() -> String {
    format!("mtseekd {}", env!("CARGO_PKG_VERSION"))
}

fn rpc(code: i64, message: String) -> RpcError {
    RpcError { code, message }
}

fn to_value(value: &impl serde::Serialize) -> Result<Value, RpcError> {
    serde_json::to_value(value).map_err(|e| rpc(code::FAILED, e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use mtseek_ipc::client::{Client, Sink};

    #[derive(Default)]
    struct Collect {
        hits: Vec<Hit>,
        notes: Vec<String>,
        stop_after: Option<usize>,
    }

    impl Sink for Collect {
        fn hit(&mut self, hit: Hit) -> ControlFlow<()> {
            self.hits.push(hit);
            if self.stop_after.is_some_and(|n| self.hits.len() >= n) {
                ControlFlow::Break(())
            } else {
                ControlFlow::Continue(())
            }
        }
        fn paths(&mut self, _: Vec<WirePath>) -> ControlFlow<()> {
            ControlFlow::Continue(())
        }
        fn error(&mut self, _: String) {}
        fn note(&mut self, message: String) {
            self.notes.push(message);
        }
    }

    fn pipe_name(test: &str) -> String {
        format!(r"\\.\pipe\mtseekd-test-{test}-{}", std::process::id())
    }

    fn params(root: &std::path::Path, index: IndexMode) -> SearchParams {
        SearchParams {
            pattern: "needle".into(),
            fixed: true,
            case: Case::Sensitive,
            roots: vec![WirePath::Text(".".into())],
            cwd: WirePath::new(root),
            ignore_files: true,
            hidden: false,
            binary: false,
            follow: false,
            mode: SearchMode::Lines,
            threads: 2,
            index,
            root_ids: None,
            global_excludes: None,
            before: 0,
            after: 0,
            max_count: None,
            positions: false,
            globs: Vec::new(),
            iglobs: Vec::new(),
            types: Vec::new(),
        }
    }

    #[test]
    fn a_search_streams_hits_and_a_cancel_stops_it() {
        let tree = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        for i in 0..200 {
            std::fs::write(tree.path().join(format!("f{i}.txt")), "a needle\n").unwrap();
        }
        let daemon =
            Daemon::spawn(Config::new(pipe_name("stream"), Some(cache.path().into()))).unwrap();
        let sid = pipe::user_sid().unwrap();
        let mut client = Client::connect(daemon.pipe(), Duration::from_secs(5), &sid)
            .unwrap()
            .unwrap();
        assert_eq!(client.info().protocol, PROTOCOL);

        let mut all = Collect::default();
        let done = client
            .search(&params(tree.path(), IndexMode::Build), &mut all)
            .unwrap()
            .expect("not cancelled");
        assert_eq!(done.summary.matched, 200);
        assert_eq!(all.hits.len(), 200);
        let first = &all.hits[0];
        assert!(first.path.to_path_buf().starts_with("."), "{first:?}");
        assert_eq!(first.lines[0].text.as_deref(), Some("a needle\n"));

        // Stop after a few hits: the client cancels, the same connection keeps working.
        let mut some = Collect {
            stop_after: Some(3),
            ..Collect::default()
        };
        let cancelled = client
            .search(&params(tree.path(), IndexMode::Off), &mut some)
            .unwrap();
        assert!(cancelled.is_none());
        let status = client.status().unwrap();
        assert_eq!(status.indexes.len(), 1, "{status:?}");
        assert_eq!(status.indexes[0].files, 200);
        daemon.stop().unwrap();
    }

    #[test]
    fn requests_before_initialize_and_unknown_methods_are_errors() {
        let cache = tempfile::tempdir().unwrap();
        let daemon =
            Daemon::spawn(Config::new(pipe_name("errors"), Some(cache.path().into()))).unwrap();
        let sid = pipe::user_sid().unwrap();
        let conn = pipe::connect(daemon.pipe(), Duration::from_secs(5), &sid)
            .unwrap()
            .unwrap();
        let mut reader = Reader::new(conn.try_clone().unwrap());
        let mut ask = |line: &str| -> RpcError {
            (&conn).write_all(line.as_bytes()).unwrap();
            let answer = reader.read_line().unwrap().unwrap().to_vec();
            Incoming::parse(&answer).unwrap().error.expect("an error")
        };
        assert_eq!(
            ask("{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"status\"}\n").code,
            code::NOT_INITIALIZED
        );
        assert_eq!(ask("not json\n").code, code::PARSE);
        assert_eq!(
            ask("{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"initialize\",\"params\":{\"client\":\"t\",\"protocol\":[7,9]}}\n").code,
            code::UNSUPPORTED_VERSION
        );
        drop(conn);
        daemon.stop().unwrap();
    }

    fn service(test: &str, cache: &std::path::Path) -> Running {
        let mut config = Config::new(pipe_name(test), Some(cache.into()));
        config.mode = Mode::Service;
        Daemon::spawn(config).unwrap()
    }

    /// What a client sends the service: which folder each root is, and its global excludes.
    fn for_service(root: &std::path::Path, index: IndexMode, excludes: &str) -> SearchParams {
        let (volume, frn) = mtseek_ntfs::identity(root).unwrap();
        SearchParams {
            root_ids: Some(vec![Some(mtseek_ipc::RootId { volume, frn })]),
            global_excludes: Some(mtseek_ipc::GlobalExcludes {
                path: None,
                b64: mtseek_ipc::base64::encode(excludes.as_bytes()),
            }),
            ..params(root, index)
        }
    }

    fn declined(result: Result<Option<SearchResult>, mtseek_ipc::client::ClientError>) -> String {
        match result {
            Err(mtseek_ipc::client::ClientError::Server(err)) if err.code == code::DECLINED => {
                err.message
            }
            other => panic!("expected a decline, got {other:?}"),
        }
    }

    #[test]
    fn the_service_answers_only_what_an_index_answers_exactly() {
        let tree = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        std::fs::write(
            tree.path().join("a.txt"),
            "a needle
",
        )
        .unwrap();
        std::fs::write(
            elsewhere.path().join("b.txt"),
            "a needle
",
        )
        .unwrap();
        let daemon = service("service", cache.path());
        let sid = pipe::user_sid().unwrap();
        let mut client = Client::connect(daemon.pipe(), Duration::from_secs(5), &sid)
            .unwrap()
            .unwrap();

        // A client that doesn't say which folders its roots are is declined, before anything
        // is built or sent.
        let mut found = Collect::default();
        let why = declined(client.search(&params(tree.path(), IndexMode::Build), &mut found));
        assert!(why.contains("folders"), "{why}");
        assert!(found.hits.is_empty() && found.notes.is_empty());

        // Registered and answered from the index.
        let mut found = Collect::default();
        let done = client
            .search(&for_service(tree.path(), IndexMode::Build, ""), &mut found)
            .unwrap()
            .unwrap();
        assert_eq!((done.summary.matched, found.hits.len()), (1, 1));
        let mut found = Collect::default();
        client
            .search(
                &for_service(
                    tree.path(),
                    IndexMode::Auto,
                    "# comments only
",
                ),
                &mut found,
            )
            .unwrap()
            .unwrap();
        assert_eq!(found.hits.len(), 1);

        // Everything an index can't answer exactly is left to the client.
        let mut found = Collect::default();
        let outside = for_service(elsewhere.path(), IndexMode::Auto, "");
        declined(client.search(&outside, &mut found));
        declined(client.search(&for_service(tree.path(), IndexMode::Off, ""), &mut found));
        let excludes = for_service(
            tree.path(),
            IndexMode::Auto,
            "*.log
",
        );
        let why = declined(client.search(&excludes, &mut found));
        assert!(why.contains("global"), "{why}");
        let mut no_ignore = excludes.clone();
        no_ignore.ignore_files = false;
        let why = declined(client.search(&no_ignore, &mut found));
        assert!(!why.contains("global"), "-u ignores the global file: {why}");
        let mut wrong = for_service(tree.path(), IndexMode::Auto, "");
        wrong.root_ids = Some(vec![Some(mtseek_ipc::RootId { volume: 1, frn: 2 })]);
        let why = declined(client.search(&wrong, &mut found));
        assert!(why.contains("another folder"), "{why}");
        assert!(found.hits.is_empty(), "declines send nothing");
        let names = NamesParams {
            pattern: "a".into(),
            case: Case::Smart,
            roots: vec![WirePath::Text(".".into())],
            cwd: WirePath::new(tree.path()),
            threads: 1,
        };
        match client.names(&names, &mut found) {
            Err(mtseek_ipc::client::ClientError::Server(err)) => {
                assert_eq!(err.code, code::DECLINED);
            }
            other => panic!("{other:?}"),
        }

        // The coverage this caller may list, and a shutdown only for an elevated admin.
        assert_eq!(client.status().unwrap().indexes.len(), 1);
        if !Token::current().unwrap().is_admin() {
            match client.shutdown() {
                Err(mtseek_ipc::client::ClientError::Server(err)) => {
                    assert_eq!(err.code, code::FAILED);
                }
                other => panic!("a non-admin stopped the service: {other:?}"),
            }
            assert!(client.status().is_ok(), "still serving");
        }
        daemon.stop().unwrap();
    }

    #[test]
    fn the_service_hides_what_the_caller_may_not_read() {
        let tree = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        std::fs::write(
            tree.path().join("open.txt"),
            "a needle
",
        )
        .unwrap();
        let locked = tree.path().join("locked.txt");
        std::fs::write(
            &locked,
            "a needle
",
        )
        .unwrap();
        let daemon = service("hides", cache.path());
        let sid = pipe::user_sid().unwrap();
        let mut client = Client::connect(daemon.pipe(), Duration::from_secs(5), &sid)
            .unwrap()
            .unwrap();
        let mut found = Collect::default();
        client
            .search(&for_service(tree.path(), IndexMode::Build, ""), &mut found)
            .unwrap()
            .unwrap();
        assert_eq!(found.hits.len(), 2);

        security::set_dacl(&locked, &format!("D:P(D;;0x1;;;{sid})(A;;FA;;;{sid})")).unwrap();
        let mut found = Collect::default();
        let done = client
            .search(&for_service(tree.path(), IndexMode::Auto, ""), &mut found)
            .unwrap()
            .unwrap();
        security::set_dacl(&locked, &format!("D:P(A;;FA;;;{sid})")).unwrap();
        let paths: Vec<PathBuf> = found.hits.iter().map(|h| h.path.to_path_buf()).collect();
        assert_eq!(paths, [PathBuf::from(".").join("open.txt")]);
        assert_eq!((done.summary.matched, done.summary.errors), (1, 1));
        daemon.stop().unwrap();
    }

    #[test]
    fn edits_are_found_and_the_delta_is_written_once_quiet() {
        let tree = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        std::fs::write(tree.path().join("a.txt"), "nothing\n").unwrap();
        let mut config = Config::new(pipe_name("quiet"), Some(cache.path().into()));
        config.quiet = Duration::from_millis(300);
        config.poll = Duration::from_millis(50);
        let daemon = Daemon::spawn(config).unwrap();
        let sid = pipe::user_sid().unwrap();
        let mut client = Client::connect(daemon.pipe(), Duration::from_secs(5), &sid)
            .unwrap()
            .unwrap();
        let mut found = Collect::default();
        client
            .search(&params(tree.path(), IndexMode::Build), &mut found)
            .unwrap();
        assert_eq!(found.hits, []);

        std::fs::write(tree.path().join("a.txt"), "a needle\n").unwrap();
        let mut found = Collect::default();
        client
            .search(&params(tree.path(), IndexMode::Auto), &mut found)
            .unwrap();
        assert_eq!(found.hits.len(), 1, "the edit is seen at once");
        // In memory first, then written once quiet.
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let status = client.status().unwrap();
            if !status.indexes[0].unsaved {
                break;
            }
            assert!(Instant::now() < deadline, "the delta was never written");
            std::thread::sleep(Duration::from_millis(50));
        }
        let deltas = std::fs::read_dir(cache.path())
            .unwrap()
            .filter(|e| {
                e.as_ref()
                    .unwrap()
                    .path()
                    .extension()
                    .is_some_and(|x| x == "delta")
            })
            .count();
        assert_eq!(deltas, 1);
        daemon.stop().unwrap();
    }
}
