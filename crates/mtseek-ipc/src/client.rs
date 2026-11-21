//! Talking to a running daemon over its named pipe.

use std::fmt;
use std::fs::File;
use std::io;
use std::ops::ControlFlow;
use std::time::Duration;

use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::{
    CancelParams, Hit, Incoming, InitializeParams, InitializeResult, MIN_PROTOCOL, Message,
    NamesParams, NamesResult, PROTOCOL, Paths, Reader, RpcError, SearchParams, SearchResult,
    StatusResult, WirePath,
};

/// The pipe of this user's daemon: `MTSEEK_PIPE` if set (tests use their own daemon),
/// else `\\.\pipe\mtseek-<SID>`.
pub fn default_pipe() -> io::Result<String> {
    if let Some(name) = std::env::var_os("MTSEEK_PIPE") {
        return Ok(name.to_string_lossy().into_owned());
    }
    mtseek_ntfs::pipe::user_pipe_name()
}

/// Why a call failed.
#[derive(Debug)]
pub enum ClientError {
    /// The pipe failed or closed.
    Io(io::Error),
    /// The server answered with an error.
    Server(RpcError),
    /// The server sent something this client doesn't understand.
    Protocol(String),
}

impl fmt::Display for ClientError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(err) => write!(f, "the daemon connection failed: {err}"),
            Self::Server(err) => f.write_str(&err.message),
            Self::Protocol(what) => write!(f, "the daemon sent {what}"),
        }
    }
}

impl std::error::Error for ClientError {}

impl From<io::Error> for ClientError {
    fn from(err: io::Error) -> Self {
        Self::Io(err)
    }
}

/// Receives what a search sends while it runs.
pub trait Sink {
    /// A matching file. [`ControlFlow::Break`] cancels the search.
    fn hit(&mut self, hit: Hit) -> ControlFlow<()>;
    /// A batch of matching names. [`ControlFlow::Break`] cancels the search.
    fn paths(&mut self, paths: Vec<WirePath>) -> ControlFlow<()>;
    /// A file or folder that couldn't be read.
    fn error(&mut self, message: String);
    /// A note for a person watching.
    fn note(&mut self, message: String);
}

/// A connection to a daemon, initialized.
#[derive(Debug)]
pub struct Client {
    writer: File,
    reader: Reader<File>,
    next_id: u64,
    info: InitializeResult,
    pipe: String,
}

impl Client {
    /// Connects to the daemon on `pipe`, which must run as `server_sid`, and agrees on a
    /// protocol version. `Ok(None)` if no daemon listens there.
    pub fn connect(
        pipe: &str,
        timeout: Duration,
        server_sid: &str,
    ) -> Result<Option<Self>, ClientError> {
        let Some(conn) = mtseek_ntfs::pipe::connect(pipe, timeout, server_sid)? else {
            return Ok(None);
        };
        let reader = Reader::new(conn.try_clone()?);
        let mut client = Self {
            writer: conn,
            reader,
            next_id: 1,
            info: InitializeResult {
                server: String::new(),
                protocol: 0,
                pid: 0,
                methods: Vec::new(),
            },
            pipe: pipe.to_owned(),
        };
        let params = InitializeParams {
            client: format!("mtseek {}", env!("CARGO_PKG_VERSION")),
            protocol: [MIN_PROTOCOL, PROTOCOL],
        };
        let info: InitializeResult = client
            .call("initialize", &params, &mut NoSink)?
            .ok_or_else(|| ClientError::Protocol("no answer to initialize".into()))?;
        if !(MIN_PROTOCOL..=PROTOCOL).contains(&info.protocol) {
            return Err(ClientError::Protocol(format!(
                "protocol version {}, which this client doesn't speak",
                info.protocol
            )));
        }
        client.info = info;
        Ok(Some(client))
    }

    /// What the server said about itself.
    pub fn info(&self) -> &InitializeResult {
        &self.info
    }

    /// The pipe it is connected to.
    pub fn pipe(&self) -> &str {
        &self.pipe
    }

    /// Runs a content search. `Ok(None)` if `sink` cancelled it.
    pub fn search(
        &mut self,
        params: &SearchParams,
        sink: &mut dyn Sink,
    ) -> Result<Option<SearchResult>, ClientError> {
        self.call("search", params, sink)
    }

    /// Runs a name search. `Ok(None)` if `sink` cancelled it.
    pub fn names(
        &mut self,
        params: &NamesParams,
        sink: &mut dyn Sink,
    ) -> Result<Option<NamesResult>, ClientError> {
        self.call("names", params, sink)
    }

    /// The daemon's open indexes.
    pub fn status(&mut self) -> Result<StatusResult, ClientError> {
        self.call("status", &(), &mut NoSink)?
            .ok_or_else(|| ClientError::Protocol("no status".into()))
    }

    /// Asks the daemon to write what it holds in memory and stop.
    pub fn shutdown(&mut self) -> Result<(), ClientError> {
        let _: Option<Value> = self.call("shutdown", &(), &mut NoSink)?;
        Ok(())
    }

    /// Sends a request and reads until its response, passing notifications to `sink`.
    fn call<R: DeserializeOwned>(
        &mut self,
        method: &str,
        params: &impl Serialize,
        sink: &mut dyn Sink,
    ) -> Result<Option<R>, ClientError> {
        let id = Value::from(self.next_id);
        self.next_id += 1;
        let mut out = Vec::new();
        crate::request(&mut out, &id, method, params);
        crate::write_message(&mut self.writer, &out)?;
        loop {
            let Some(line) = self.reader.read_line()? else {
                return Err(ClientError::Io(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "the daemon closed the connection",
                )));
            };
            let message = Incoming::parse(line)
                .map_err(|e| ClientError::Protocol(format!("bad JSON: {e}")))?;
            let bad = |e: serde_json::Error| ClientError::Protocol(format!("a bad message: {e}"));
            if let Some(name) = message.method.as_deref() {
                let flow = match name {
                    "$/hit" => {
                        let hit: Hit = message.params().map_err(bad)?;
                        if hit.id == id {
                            sink.hit(hit)
                        } else {
                            ControlFlow::Continue(())
                        }
                    }
                    "$/paths" => {
                        let paths: Paths = message.params().map_err(bad)?;
                        if paths.id == id {
                            sink.paths(paths.paths)
                        } else {
                            ControlFlow::Continue(())
                        }
                    }
                    "$/error" | "$/note" => {
                        let m: Message = message.params().map_err(bad)?;
                        if m.id == id {
                            if name == "$/error" {
                                sink.error(m.message);
                            } else {
                                sink.note(m.message);
                            }
                        }
                        ControlFlow::Continue(())
                    }
                    // Notifications this version doesn't know are ignored.
                    _ => ControlFlow::Continue(()),
                };
                if flow.is_break() {
                    let mut out = Vec::new();
                    crate::notification(&mut out, "$/cancel", &CancelParams { id });
                    // The server also stops when the connection closes, so a failed send
                    // is fine.
                    let _ = crate::write_message(&mut self.writer, &out);
                    return Ok(None);
                }
                continue;
            }
            if message.id.as_ref() != Some(&id) {
                continue;
            }
            if let Some(err) = message.error {
                return Err(ClientError::Server(err));
            }
            let raw = message.result.map_or("null", |r| r.get());
            return serde_json::from_str(raw).map(Some).map_err(bad);
        }
    }
}

/// For calls that send no notifications.
struct NoSink;

impl Sink for NoSink {
    fn hit(&mut self, _: Hit) -> ControlFlow<()> {
        ControlFlow::Continue(())
    }
    fn paths(&mut self, _: Vec<WirePath>) -> ControlFlow<()> {
        ControlFlow::Continue(())
    }
    fn error(&mut self, _: String) {}
    fn note(&mut self, _: String) {}
}
