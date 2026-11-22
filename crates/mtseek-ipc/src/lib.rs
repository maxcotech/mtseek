//! The daemon's protocol (ADR 0013): JSON-RPC 2.0, one message per line.
//!
//! * **Framing:** each message is one JSON object, UTF-8, ended by `\n` ([`Reader`],
//!   [`write_message`]).
//! * **Methods:** `initialize` first, then `search` (with `$/hit`, `$/error` and `$/note`
//!   notifications while it runs), `names` (with `$/paths`), `status`, `shutdown`, and the
//!   `$/cancel` notification.
//! * **Exact bytes:** a matched line is `{"text": ...}` when it is UTF-8 and `{"b64": ...}`
//!   otherwise; a path is a string when it is valid Unicode and `{"wide": [u16...]}`
//!   otherwise ([`Line`], [`WirePath`]). Output in daemon mode is byte-for-byte what direct
//!   mode prints.
//! * **Versions:** one integer. The client offers a range and the server picks the highest
//!   it shares ([`negotiate`]). Optional fields can be added without a new version; both
//!   sides ignore fields they don't know. An option that changes what a search reports
//!   always needs a new version, because a server that ignored it would answer wrongly
//!   (ADR 0015 §4). Version 2 added context, `-m`, counts, match positions, globs and
//!   types; version 3 `--no-ignore-vcs` (ADR 0016). [`SearchParams::version`] says which
//!   version a search needs.
//!
//! This crate knows nothing about indexes, so clients can depend on it alone. [`client`]
//! (Windows) connects over the named pipe.

use std::ffi::OsString;
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use serde_json::value::RawValue;

pub mod base64;
#[cfg(windows)]
pub mod client;

/// The newest protocol version this crate speaks.
pub const PROTOCOL: u32 = 3;
/// The oldest protocol version this crate speaks.
pub const MIN_PROTOCOL: u32 = 1;
/// A message longer than this is refused (the connection is closed).
pub const MAX_LINE: usize = 64 << 20;

/// The version both sides speak: the highest in both ranges, if any.
pub fn negotiate(ours: [u32; 2], theirs: [u32; 2]) -> Option<u32> {
    let low = ours[0].max(theirs[0]);
    let high = ours[1].min(theirs[1]);
    (low <= high).then_some(high)
}

/// A path as sent: a string when it is valid Unicode, else its UTF-16 code units (Windows
/// allows unpaired surrogates in names).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum WirePath {
    /// Valid Unicode.
    Text(String),
    /// Anything else.
    Wide {
        /// The UTF-16 code units.
        wide: Vec<u16>,
    },
}

impl WirePath {
    /// The wire form of `path`.
    pub fn new(path: &Path) -> Self {
        if let Some(text) = path.to_str() {
            return Self::Text(text.to_owned());
        }
        #[cfg(windows)]
        {
            use std::os::windows::ffi::OsStrExt;
            Self::Wide {
                wide: path.as_os_str().encode_wide().collect(),
            }
        }
        #[cfg(not(windows))]
        Self::Text(path.to_string_lossy().into_owned())
    }

    /// The path it stands for.
    pub fn to_path_buf(&self) -> PathBuf {
        match self {
            Self::Text(text) => PathBuf::from(text),
            #[cfg(windows)]
            Self::Wide { wide } => {
                use std::os::windows::ffi::OsStringExt;
                PathBuf::from(OsString::from_wide(wide))
            }
            #[cfg(not(windows))]
            Self::Wide { wide } => PathBuf::from(OsString::from(String::from_utf16_lossy(wide))),
        }
    }
}

/// One reported line: its number, its exact bytes and, from version 2, where it is, whether
/// it is a context line and where its matches are.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Line {
    /// 1-based line number.
    pub n: u64,
    /// The bytes, when they are UTF-8.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// The bytes in base64, when they aren't.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub b64: Option<String>,
    /// Where the line starts in the searched text, in bytes.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub off: u64,
    /// A context line, not a matching one.
    #[serde(default, skip_serializing_if = "is_false")]
    pub ctx: bool,
    /// Where each match is in the line: [start, end] (with `positions`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub m: Vec<[u64; 2]>,
}

impl Line {
    /// The wire form of matching line `n` with bytes `bytes`.
    pub fn new(n: u64, bytes: &[u8]) -> Self {
        let (text, b64) = match std::str::from_utf8(bytes) {
            Ok(text) => (Some(text.to_owned()), None),
            Err(_) => (None, Some(base64::encode(bytes))),
        };
        Self {
            n,
            text,
            b64,
            off: 0,
            ctx: false,
            m: Vec::new(),
        }
    }

    /// The line's bytes. `None` if the message is malformed.
    pub fn bytes(&self) -> Option<Vec<u8>> {
        match (&self.text, &self.b64) {
            (Some(text), None) => Some(text.as_bytes().to_vec()),
            (None, Some(b64)) => base64::decode(b64),
            _ => None,
        }
    }
}

/// `initialize` parameters.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InitializeParams {
    /// The client's name and version, for logs.
    pub client: String,
    /// The protocol versions it speaks: [oldest, newest].
    pub protocol: [u32; 2],
}

/// `initialize` result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InitializeResult {
    /// The server's name and version.
    pub server: String,
    /// The protocol version chosen.
    pub protocol: u32,
    /// The server's process id.
    pub pid: u32,
    /// The methods it answers.
    pub methods: Vec<String>,
}

/// How a pattern compares case.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Case {
    /// Exact case.
    Sensitive,
    /// Any case.
    Insensitive,
    /// Any case unless the pattern has an uppercase letter.
    Smart,
}

/// What a content search reports per file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SearchMode {
    /// Paths only (`-l`).
    Files,
    /// Matching lines, with context lines if asked for.
    Lines,
    /// Matching lines per file (`-c`). Version 2.
    Count,
    /// Matches per file (`--count-matches`). Version 2.
    CountMatches,
}

/// One `-t` or `-T` file type.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TypeRule {
    /// The type's name.
    pub name: String,
    /// `-T`: leave its files out.
    #[serde(default, skip_serializing_if = "is_false")]
    pub not: bool,
}

/// Whether a content search uses the index.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum IndexMode {
    /// Use a covering index if there is one.
    #[default]
    Auto,
    /// `--index`.
    Build,
    /// `--reindex`.
    Rebuild,
    /// `--no-index`.
    Off,
}

/// `search` parameters: exactly what the CLI parsed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[allow(clippy::struct_excessive_bools)] // one field per ripgrep flag
pub struct SearchParams {
    /// The pattern.
    pub pattern: String,
    /// Literal text (`-F`) rather than a regex.
    pub fixed: bool,
    /// Case handling.
    pub case: Case,
    /// Files or folders to search, as typed.
    pub roots: Vec<WirePath>,
    /// The client's working directory: relative roots are found from it.
    pub cwd: WirePath,
    /// Honour ignore files.
    pub ignore_files: bool,
    /// Include hidden files.
    pub hidden: bool,
    /// Search binary files.
    pub binary: bool,
    /// Follow links.
    pub follow: bool,
    /// Paths or lines.
    pub mode: SearchMode,
    /// Worker threads (0: the default).
    #[serde(default)]
    pub threads: usize,
    /// Index use.
    #[serde(default)]
    pub index: IndexMode,
    /// Which folder each root is, as the client opened it (ADR 0014): the service checks
    /// that the same path names the same folder for it. `None` for a root the client
    /// couldn't open.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root_ids: Option<Vec<Option<RootId>>>,
    /// Git's global excludes file as the client's `rg` would read it (ADR 0014 §5).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub global_excludes: Option<GlobalExcludes>,
    /// Version 2: lines of context before each match (`-B`).
    #[serde(default, skip_serializing_if = "is_zero_usize")]
    pub before: usize,
    /// Version 2: lines of context after each match (`-A`).
    #[serde(default, skip_serializing_if = "is_zero_usize")]
    pub after: usize,
    /// Version 2: matching lines per file at most (`-m`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_count: Option<u64>,
    /// Version 2: report where each match is in its line (`--json`).
    #[serde(default, skip_serializing_if = "is_false")]
    pub positions: bool,
    /// Version 2: `-g` globs, matched relative to `cwd`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub globs: Vec<String>,
    /// Version 2: `--iglob` globs.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub iglobs: Vec<String>,
    /// Version 2: `-t` and `-T`, in order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub types: Vec<TypeRule>,
    /// Version 3: with `ignore_files`, leave out git's ignore files (`--no-ignore-vcs`).
    #[serde(default, skip_serializing_if = "is_false")]
    pub no_ignore_vcs: bool,
}

impl SearchParams {
    /// The oldest protocol version that carries this search exactly. A server that speaks
    /// an older one must not get it: it would ignore what it doesn't know and answer a
    /// different search.
    pub fn version(&self) -> u32 {
        let v2 = matches!(self.mode, SearchMode::Count | SearchMode::CountMatches)
            || self.before > 0
            || self.after > 0
            || self.max_count.is_some()
            || self.positions
            || !self.globs.is_empty()
            || !self.iglobs.is_empty()
            || !self.types.is_empty();
        if self.no_ignore_vcs {
            3
        } else if v2 {
            2
        } else {
            1
        }
    }
}

/// A folder's identity: its volume's serial number and its file reference number.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RootId {
    /// The volume serial number.
    pub volume: u32,
    /// The file reference number.
    pub frn: u64,
}

/// Git's global excludes file of a client.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GlobalExcludes {
    /// Where it is, if git's configuration names one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<WirePath>,
    /// Its contents, in base64 (empty if it doesn't exist).
    #[serde(default)]
    pub b64: String,
}

/// `$/hit`: a matching file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hit {
    /// The `search` request it belongs to.
    pub id: Value,
    /// The path, joined onto the root as typed.
    pub path: WirePath,
    /// Matching and context lines (only in `lines` mode).
    #[serde(default)]
    pub lines: Vec<Line>,
    /// Set when the file turned out to be binary after it matched: the NUL's offset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binary_offset: Option<u64>,
    /// Version 2: matching lines in the file.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub matched_lines: u64,
    /// Version 2: matches in the file (counted for `count_matches` and `positions`).
    #[serde(default, skip_serializing_if = "is_zero")]
    pub matches: u64,
    /// Version 2: bytes searched in the file.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub bytes: u64,
    /// Version 2: how long searching the file took, in nanoseconds.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub nanos: u64,
}

/// `$/error` (a file that couldn't be read) and `$/note` (a note for a person watching).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Message {
    /// The request it belongs to.
    pub id: Value,
    /// The text, as the CLI prints it after `mtseek: `.
    pub message: String,
}

/// Totals of a content search.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Summary {
    /// Files searched.
    pub searched: u64,
    /// Files with a match.
    pub matched: u64,
    /// Files or folders that couldn't be read.
    pub errors: u64,
    /// Version 2: bytes searched.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub bytes: u64,
    /// Version 2: time spent searching files, added up, in nanoseconds.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub nanos: u64,
}

#[allow(clippy::trivially_copy_pass_by_ref)] // serde passes fields by reference
fn is_zero(n: &u64) -> bool {
    *n == 0
}

#[allow(clippy::trivially_copy_pass_by_ref)] // serde passes fields by reference
fn is_zero_usize(n: &usize) -> bool {
    *n == 0
}

#[allow(clippy::trivially_copy_pass_by_ref)] // serde passes fields by reference
fn is_false(b: &bool) -> bool {
    !*b
}

/// `search` result.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct SearchResult {
    /// Totals.
    pub summary: Summary,
    /// Timings and sizes, for `--stats`.
    #[serde(default)]
    pub stats: Vec<String>,
}

/// `names` parameters.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NamesParams {
    /// Space-separated terms.
    pub pattern: String,
    /// Case handling.
    pub case: Case,
    /// Folders to search under, as typed.
    pub roots: Vec<WirePath>,
    /// The client's working directory.
    pub cwd: WirePath,
    /// Worker threads (0: all CPUs).
    #[serde(default)]
    pub threads: usize,
}

/// `$/paths`: a batch of matching names.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Paths {
    /// The `names` request it belongs to.
    pub id: Value,
    /// Absolute paths, in order.
    pub paths: Vec<WirePath>,
}

/// `names` result.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct NamesResult {
    /// How many paths matched.
    pub matched: u64,
    /// Timings and sizes, for `--stats`.
    #[serde(default)]
    pub stats: Vec<String>,
}

/// `$/cancel` parameters.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CancelParams {
    /// The request to stop.
    pub id: Value,
}

/// One content index in `status`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexStatus {
    /// The coverage root.
    pub root: WirePath,
    /// Whether it honours ignore files.
    pub ignore_files: bool,
    /// Whether it includes hidden files.
    pub hidden: bool,
    /// Files in it.
    pub files: u64,
    /// Bytes on disk.
    pub file_bytes: u64,
    /// Whether changes are waiting in memory.
    pub unsaved: bool,
    /// Journal positions: (drive letter, next USN).
    pub checkpoints: Vec<(char, i64)>,
}

/// One name index in `status`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NamesStatus {
    /// Its root.
    pub root: WirePath,
    /// Entries in it.
    pub entries: u64,
}

/// `status` result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusResult {
    /// The server's name and version.
    pub server: String,
    /// Its process id.
    pub pid: u32,
    /// Its cache directory.
    pub cache: Option<WirePath>,
    /// Open content indexes.
    pub indexes: Vec<IndexStatus>,
    /// Name indexes in memory.
    pub names: Vec<NamesStatus>,
}

/// A JSON-RPC error object.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RpcError {
    /// One of the `code` constants.
    pub code: i64,
    /// What went wrong. For a failed search, the text the CLI prints after `mtseek: `.
    pub message: String,
}

/// Error codes.
pub mod code {
    /// The line isn't JSON.
    pub const PARSE: i64 = -32700;
    /// Not a valid request.
    pub const INVALID_REQUEST: i64 = -32600;
    /// No such method.
    pub const METHOD_NOT_FOUND: i64 = -32601;
    /// Bad parameters.
    pub const INVALID_PARAMS: i64 = -32602;
    /// The search ran and failed (bad pattern, unreadable cache...).
    pub const FAILED: i64 = -32000;
    /// No protocol version in common.
    pub const UNSUPPORTED_VERSION: i64 = -32001;
    /// A request before `initialize`.
    pub const NOT_INITIALIZED: i64 = -32002;
    /// The service won't answer this search exactly: the client runs it itself
    /// (ADR 0014 §6). Nothing was sent for it.
    pub const DECLINED: i64 = -32003;
    /// Stopped by `$/cancel`.
    pub const CANCELLED: i64 = -32800;
}

/// A message as received. Requests have `id` and `method`, notifications only `method`,
/// responses `id` and `result` or `error`.
#[derive(Debug, Deserialize)]
pub struct Incoming<'a> {
    /// Request or response id.
    #[serde(default)]
    pub id: Option<Value>,
    /// Method name.
    #[serde(default, borrow)]
    pub method: Option<std::borrow::Cow<'a, str>>,
    /// Parameters, parsed by the receiver once it knows the method.
    #[serde(default, borrow)]
    pub params: Option<&'a RawValue>,
    /// A successful result.
    #[serde(default, borrow)]
    pub result: Option<&'a RawValue>,
    /// A failure.
    #[serde(default)]
    pub error: Option<RpcError>,
}

impl<'a> Incoming<'a> {
    /// Parses one line.
    pub fn parse(line: &'a [u8]) -> serde_json::Result<Self> {
        serde_json::from_slice(line)
    }

    /// The parameters as `T` (absent parameters parse as `null`).
    pub fn params<T: Deserialize<'a>>(&self) -> serde_json::Result<T> {
        match self.params {
            Some(raw) => serde_json::from_str(raw.get()),
            None => serde_json::from_str("null"),
        }
    }
}

#[derive(Serialize)]
struct Request<'a, P> {
    jsonrpc: &'static str,
    id: &'a Value,
    method: &'a str,
    params: &'a P,
}

#[derive(Serialize)]
struct Notification<'a, P> {
    jsonrpc: &'static str,
    method: &'a str,
    params: &'a P,
}

#[derive(Serialize)]
struct Response<'a, R> {
    jsonrpc: &'static str,
    id: &'a Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<&'a R>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<&'a RpcError>,
}

/// Appends a request line to `out`.
pub fn request(out: &mut Vec<u8>, id: &Value, method: &str, params: &impl Serialize) {
    push(
        out,
        &Request {
            jsonrpc: "2.0",
            id,
            method,
            params,
        },
    );
}

/// Appends a notification line to `out`.
pub fn notification(out: &mut Vec<u8>, method: &str, params: &impl Serialize) {
    push(
        out,
        &Notification {
            jsonrpc: "2.0",
            method,
            params,
        },
    );
}

/// Appends a success response line to `out`.
pub fn result(out: &mut Vec<u8>, id: &Value, result: &impl Serialize) {
    push(
        out,
        &Response {
            jsonrpc: "2.0",
            id,
            result: Some(result),
            error: None,
        },
    );
}

/// Appends an error response line to `out`. `id` is `null` if the request's id is unknown.
pub fn error(out: &mut Vec<u8>, id: &Value, error: &RpcError) {
    push(
        out,
        &Response::<()> {
            jsonrpc: "2.0",
            id,
            result: None,
            error: Some(error),
        },
    );
}

fn push(out: &mut Vec<u8>, message: &impl Serialize) {
    // Serializing these types to memory can't fail: string keys, no maps with other keys.
    serde_json::to_writer(&mut *out, message).expect("messages serialize");
    out.push(b'\n');
}

/// Writes `bytes` (one or more lines from [`request`] and the like) to `out`.
pub fn write_message(out: &mut impl Write, bytes: &[u8]) -> io::Result<()> {
    out.write_all(bytes)?;
    out.flush()
}

/// Reads messages one line at a time.
#[derive(Debug)]
pub struct Reader<R> {
    inner: io::BufReader<R>,
    line: Vec<u8>,
}

impl<R: io::Read> Reader<R> {
    /// Reads from `inner`.
    pub fn new(inner: R) -> Self {
        Self {
            inner: io::BufReader::with_capacity(64 * 1024, inner),
            line: Vec::new(),
        }
    }

    /// The next line, without its `\n`. `None` at the end of the stream. A line longer than
    /// [`MAX_LINE`], or one cut off by the end of the stream, is an error.
    pub fn read_line(&mut self) -> io::Result<Option<&[u8]>> {
        self.line.clear();
        loop {
            let buf = self.inner.fill_buf()?;
            if buf.is_empty() {
                if self.line.is_empty() {
                    return Ok(None);
                }
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "the stream ended inside a message",
                ));
            }
            let (part, done) = match memchr(buf) {
                Some(i) => (&buf[..i], i + 1),
                None => (buf, buf.len()),
            };
            if self.line.len() + part.len() > MAX_LINE {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "a message is too long",
                ));
            }
            self.line.extend_from_slice(part);
            let found = done > part.len();
            self.inner.consume(done);
            if found {
                return Ok(Some(&self.line));
            }
        }
    }

    /// Whether a whole or partial message is buffered already.
    pub fn buffered(&self) -> bool {
        !self.inner.buffer().is_empty()
    }

    /// The stream it reads from.
    pub fn get_ref(&self) -> &R {
        self.inner.get_ref()
    }
}

fn memchr(buf: &[u8]) -> Option<usize> {
    buf.iter().position(|&b| b == b'\n')
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn versions_are_negotiated_to_the_highest_shared() {
        assert_eq!(negotiate([1, 1], [1, 1]), Some(1));
        assert_eq!(negotiate([1, 3], [2, 5]), Some(3));
        assert_eq!(negotiate([1, 1], [2, 3]), None);
    }

    fn plain_search() -> SearchParams {
        SearchParams {
            pattern: "needle".into(),
            fixed: true,
            case: Case::Sensitive,
            roots: vec![WirePath::Text(".".into())],
            cwd: WirePath::Text(r"C:\".into()),
            ignore_files: true,
            hidden: false,
            binary: false,
            follow: false,
            mode: SearchMode::Lines,
            threads: 0,
            index: IndexMode::Auto,
            root_ids: None,
            global_excludes: None,
            before: 0,
            after: 0,
            max_count: None,
            positions: false,
            globs: Vec::new(),
            iglobs: Vec::new(),
            types: Vec::new(),
            no_ignore_vcs: false,
        }
    }

    /// A version 1 search is sent exactly as version 1 sent it, and every option of
    /// version 2 makes the search need version 2.
    #[test]
    fn a_search_needs_version_2_only_for_what_version_2_added() {
        let plain = plain_search();
        assert_eq!(plain.version(), 1);
        let json = serde_json::to_string(&plain).unwrap();
        for v2 in [
            "before",
            "after",
            "max_count",
            "positions",
            "globs",
            "iglobs",
            "types",
        ] {
            assert!(!json.contains(v2), "{json}");
        }
        let needs = |change: &dyn Fn(&mut SearchParams)| {
            let mut p = plain_search();
            change(&mut p);
            p.version()
        };
        assert_eq!(needs(&|p| p.mode = SearchMode::Count), 2);
        assert_eq!(needs(&|p| p.mode = SearchMode::CountMatches), 2);
        assert_eq!(needs(&|p| p.mode = SearchMode::Files), 1);
        assert_eq!(needs(&|p| p.before = 1), 2);
        assert_eq!(needs(&|p| p.after = 1), 2);
        assert_eq!(needs(&|p| p.max_count = Some(0)), 2);
        assert_eq!(needs(&|p| p.positions = true), 2);
        assert_eq!(needs(&|p| p.globs = vec!["*.rs".into()]), 2);
        assert_eq!(needs(&|p| p.iglobs = vec!["*.rs".into()]), 2);
        let rust = TypeRule {
            name: "rust".into(),
            not: false,
        };
        assert_eq!(needs(&|p| p.types = vec![rust.clone()]), 2);
        assert_eq!(needs(&|p| p.no_ignore_vcs = true), 3);
        let mode = serde_json::to_string(&SearchMode::CountMatches).unwrap();
        assert_eq!(mode, r#""count_matches""#);
    }

    #[test]
    fn lines_are_text_when_utf8_and_base64_otherwise() {
        let text = Line::new(3, "café\r\n".as_bytes());
        assert_eq!(
            serde_json::to_string(&text).unwrap(),
            r#"{"n":3,"text":"café\r\n"}"#
        );
        let binary = Line::new(4, b"a\xffb\n");
        assert_eq!(
            serde_json::to_string(&binary).unwrap(),
            r#"{"n":4,"b64":"Yf9iCg=="}"#
        );
        let both: Line = serde_json::from_str(r#"{"n":1,"text":"a","b64":"YQ=="}"#).unwrap();
        assert_eq!(both.bytes(), None, "ambiguous lines are malformed");
    }

    #[cfg(windows)]
    #[test]
    fn paths_that_are_not_unicode_travel_as_utf16() {
        use std::os::windows::ffi::OsStringExt;
        let odd = PathBuf::from(OsString::from_wide(&[0x61, 0xD800, 0x62]));
        let wire = WirePath::new(&odd);
        let json = serde_json::to_string(&wire).unwrap();
        assert_eq!(json, r#"{"wide":[97,55296,98]}"#);
        let back: WirePath = serde_json::from_str(&json).unwrap();
        assert_eq!(back.to_path_buf(), odd);
        let plain: WirePath = serde_json::from_str(r#""C:\\x""#).unwrap();
        assert_eq!(plain.to_path_buf(), PathBuf::from(r"C:\x"));
    }

    #[test]
    fn messages_round_trip_and_unknown_fields_are_ignored() {
        let mut out = Vec::new();
        let id = Value::from(7);
        request(
            &mut out,
            &id,
            "search",
            &CancelParams { id: Value::from(1) },
        );
        notification(&mut out, "$/cancel", &CancelParams { id: Value::from(7) });
        result(&mut out, &id, &Summary::default());
        error(
            &mut out,
            &Value::Null,
            &RpcError {
                code: code::PARSE,
                message: "x".into(),
            },
        );
        let mut reader = Reader::new(&out[..]);
        let line = reader.read_line().unwrap().unwrap().to_vec();
        let m = Incoming::parse(&line).unwrap();
        assert_eq!(
            (m.id, m.method.as_deref()),
            (Some(id.clone()), Some("search"))
        );
        let line = reader.read_line().unwrap().unwrap().to_vec();
        let m = Incoming::parse(&line).unwrap();
        assert!(m.id.is_none());
        assert_eq!(m.params::<CancelParams>().unwrap().id, Value::from(7));
        let line = reader.read_line().unwrap().unwrap().to_vec();
        assert!(Incoming::parse(&line).unwrap().result.is_some());
        let line = reader.read_line().unwrap().unwrap().to_vec();
        assert_eq!(
            Incoming::parse(&line).unwrap().error.unwrap().code,
            code::PARSE
        );
        assert!(reader.read_line().unwrap().is_none());

        let newer = br#"{"jsonrpc":"2.0","id":1,"result":{"searched":1,"matched":1,"errors":0,"later":true}}"#;
        let m = Incoming::parse(newer).unwrap();
        let summary: Summary = serde_json::from_str(m.result.unwrap().get()).unwrap();
        assert_eq!(summary.matched, 1);
    }

    #[test]
    fn a_cut_or_oversized_message_is_an_error() {
        let mut reader = Reader::new(&b"{\"a\":1}\n{\"b\""[..]);
        assert!(reader.read_line().unwrap().is_some());
        assert_eq!(
            reader.read_line().unwrap_err().kind(),
            io::ErrorKind::UnexpectedEof
        );
        let big = vec![b'x'; MAX_LINE + 1];
        let mut reader = Reader::new(&big[..]);
        assert_eq!(
            reader.read_line().unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }

    proptest! {
        #[test]
        fn any_line_bytes_survive_the_wire(bytes in proptest::collection::vec(any::<u8>(), 0..300), n in any::<u64>()) {
            let json = serde_json::to_string(&Line::new(n, &bytes)).unwrap();
            prop_assert!(!json.contains('\n'), "one message per line");
            let back: Line = serde_json::from_str(&json).unwrap();
            prop_assert_eq!(back.n, n);
            prop_assert_eq!(back.bytes(), Some(bytes));
        }

        /// Other users' requests are untrusted input for the service (ADR 0014): a damaged
        /// request, cut or with any byte changed, is refused or parsed, never a panic.
        #[test]
        fn damaged_requests_are_refused_or_parsed(at in any::<prop::sample::Index>(), value in any::<u8>(), cut in any::<prop::sample::Index>()) {
            let request = SearchParams {
                pattern: "ne(e)dle".into(),
                fixed: false,
                case: Case::Smart,
                roots: vec![WirePath::Text(r"C:\Dev".into()), WirePath::Wide { wide: vec![0x61, 0xD800] }],
                cwd: WirePath::Text(r"C:\".into()),
                ignore_files: true,
                hidden: false,
                binary: false,
                follow: false,
                mode: SearchMode::Lines,
                threads: 0,
                index: IndexMode::Auto,
                root_ids: Some(vec![Some(RootId { volume: 7, frn: 9 }), None]),
                global_excludes: Some(GlobalExcludes { path: None, b64: base64::encode(b"*.log
    ") }),
                before: 2,
                after: 1,
                max_count: Some(3),
                positions: true,
                globs: vec!["*.rs".into(), "!target".into()],
                iglobs: vec!["*.MD".into()],
                types: vec![TypeRule { name: "rust".into(), not: true }],
                no_ignore_vcs: true,
            };
            let mut line = Vec::new();
            super::request(&mut line, &Value::from(1), "search", &request);
            let i = at.index(line.len());
            line[i] = value;
            line.truncate(cut.index(line.len() + 1));
            if let Ok(message) = Incoming::parse(&line) {
                let _ = message.params::<SearchParams>();
                let _ = message.params::<InitializeParams>();
                let _ = message.params::<CancelParams>();
            }
            let mut reader = Reader::new(&line[..]);
            while let Ok(Some(_)) = reader.read_line() {}
        }
    }
}
