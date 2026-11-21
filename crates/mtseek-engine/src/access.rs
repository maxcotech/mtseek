//! Who a search runs for (ADR 0014).
//!
//! Every host checks what its caller may see against the security descriptors the index
//! recorded, so the index can keep the bytes of a file whose permissions changed after it
//! was read, and still show the caller exactly what its own `rg` would:
//!
//! * direct mode: this process's own token ([`Caller::process`])
//! * a console daemon: its client's token, from the pipe
//! * the service: its client's token, strictly: a descriptor the index couldn't record
//!   grants nothing
//!
//! Decisions are cached per descriptor; there are few distinct ones (1 for all of `C:\Dev`).

use std::collections::HashMap;
use std::sync::{Mutex, PoisonError};

use mtseek_query::Viewer;

/// The account a search runs for, and what it may open.
#[derive(Debug)]
pub struct Caller {
    #[cfg(windows)]
    token: Option<mtseek_ntfs::security::Token>,
    /// Whether an unknown (empty) descriptor grants nothing.
    strict: bool,
    decided: Mutex<HashMap<(Vec<u8>, u32), bool>>,
}

impl Caller {
    /// This process. An unknown descriptor grants access: the index was built by this same
    /// account, which read the file or listed the folder to index it.
    pub fn process() -> Self {
        Self {
            #[cfg(windows)]
            token: mtseek_ntfs::security::Token::current().ok(),
            strict: false,
            decided: Mutex::default(),
        }
    }

    /// A daemon's client. `strict` for the service, whose index another account built.
    #[cfg(windows)]
    pub fn client(token: mtseek_ntfs::security::Token, strict: bool) -> Self {
        Self {
            token: Some(token),
            strict,
            decided: Mutex::default(),
        }
    }

    /// Whether the caller is an elevated administrator.
    pub fn is_admin(&self) -> bool {
        #[cfg(windows)]
        {
            self.token
                .as_ref()
                .is_some_and(mtseek_ntfs::security::Token::is_admin)
        }
        #[cfg(not(windows))]
        {
            false
        }
    }

    /// The caller's user SID, if known.
    pub fn user_sid(&self) -> Option<String> {
        #[cfg(windows)]
        {
            self.token.as_ref().and_then(|t| t.user_sid().ok())
        }
        #[cfg(not(windows))]
        {
            None
        }
    }

    fn allows(&self, sd: &[u8], desired: u32) -> bool {
        if sd.is_empty() {
            return !self.strict;
        }
        let key = (sd.to_vec(), desired);
        if let Some(&known) = self
            .decided
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&key)
        {
            return known;
        }
        let allowed = self.check(sd, desired);
        self.decided
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(key, allowed);
        allowed
    }

    #[cfg(windows)]
    fn check(&self, sd: &[u8], desired: u32) -> bool {
        match &self.token {
            Some(token) => token.allows(sd, desired),
            // Without a token of our own nothing can be checked: as before descriptors.
            None => !self.strict,
        }
    }

    #[cfg(not(windows))]
    fn check(&self, _: &[u8], _: u32) -> bool {
        !self.strict
    }
}

#[cfg(windows)]
const READ: u32 = mtseek_ntfs::security::READ;
#[cfg(windows)]
const LIST: u32 = mtseek_ntfs::security::LIST;
#[cfg(not(windows))]
const READ: u32 = 1;
#[cfg(not(windows))]
const LIST: u32 = 2;

impl Viewer for Caller {
    fn may_read(&self, sd: &[u8]) -> bool {
        self.allows(sd, READ)
    }

    fn may_list(&self, sd: &[u8]) -> bool {
        self.allows(sd, LIST)
    }
}
