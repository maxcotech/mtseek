use std::path::PathBuf;

/// Errors that cross crate boundaries.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// The search pattern could not be compiled.
    #[error("invalid pattern: {0}")]
    InvalidPattern(String),

    /// A flag's value is invalid: a glob that doesn't parse, an unknown file type. The text
    /// is the whole message.
    #[error("{0}")]
    InvalidArgument(String),

    /// The daemon won't answer this search exactly (ADR 0014 §6). The caller runs it
    /// itself; nothing was reported yet.
    #[error("not answered by the daemon: {0}")]
    Declined(String),

    /// An I/O error tied to a specific path.
    #[error("{}: {source}", path.display())]
    Io {
        /// The path being read or listed.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: std::io::Error,
    },
}

/// Shorthand for `Result<T, mtseek_core::Error>`.
pub type Result<T, E = Error> = std::result::Result<T, E>;
