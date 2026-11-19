//! Shared vocabulary for every mtseek crate.
//!
//! * [`Query`]: what a search looks for (pattern, literal or regex, case handling).
//! * [`Scope`]: which files a search covers (roots plus ripgrep-compatible filters).
//! * [`Error`]: the error type shared across crate boundaries.
//!
//! This crate has no I/O and no heavy dependencies. Every other crate depends on it,
//! so keep it small and stable.

mod error;
mod query;
mod scope;

pub use error::{Error, Result};
pub use query::{CaseMode, PatternKind, Query};
pub use scope::{Filter, Scope};
