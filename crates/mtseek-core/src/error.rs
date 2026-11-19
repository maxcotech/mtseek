use std::path::PathBuf;

/// Errors that cross crate boundaries.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// The search pattern could not be compiled.
    #[error("invalid pattern: {0}")]
    InvalidPattern(String),

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
