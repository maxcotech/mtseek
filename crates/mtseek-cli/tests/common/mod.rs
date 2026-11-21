//! Where a test's `mtseek` runs: in its own process with its own cache ("direct"), or
//! through a daemon started for the test, in this process, on its own pipe and cache
//! (ADR 0013). Both must print the same.

#![allow(dead_code)] // each test file uses part of it

use std::path::Path;
use std::process::Command;

/// A pipe nothing listens on: direct runs set it, so they never reach a daemon the
/// developer happens to run.
pub(crate) const NO_DAEMON: &str = r"\\.\pipe\mtseek-test-no-daemon";

/// A cache, and maybe a daemon serving from it.
pub(crate) struct How {
    // Declared first, so it stops before the cache is deleted.
    #[cfg(windows)]
    daemon: Option<mtseek_daemon::Running>,
    cache: tempfile::TempDir,
}

impl How {
    /// `mtseek` searches in its own process.
    pub(crate) fn direct() -> Self {
        Self {
            #[cfg(windows)]
            daemon: None,
            cache: tempfile::tempdir().expect("temp dir"),
        }
    }

    /// `mtseek --daemon` asks a daemon started for this test. `name` makes its pipe unique.
    #[cfg(windows)]
    pub(crate) fn daemon(name: &str) -> Self {
        let cache = tempfile::tempdir().expect("temp dir");
        let pipe = format!(r"\\.\pipe\mtseek-test-{name}-{}", std::process::id());
        let config = mtseek_daemon::Config::new(pipe, Some(cache.path().to_path_buf()));
        Self {
            daemon: Some(mtseek_daemon::Daemon::spawn(config).expect("the daemon starts")),
            cache,
        }
    }

    /// Both ways, direct first.
    pub(crate) fn both(name: &str) -> Vec<Self> {
        #[cfg(windows)]
        return vec![Self::direct(), Self::daemon(name)];
        #[cfg(not(windows))]
        {
            let _ = name;
            vec![Self::direct()]
        }
    }

    /// The cache direct runs use (the daemon has the same one).
    pub(crate) fn cache(&self) -> &Path {
        self.cache.path()
    }

    /// Whether runs go through a daemon.
    pub(crate) fn is_daemon(&self) -> bool {
        #[cfg(windows)]
        return self.daemon.is_some();
        #[cfg(not(windows))]
        false
    }

    /// A short label for failure messages.
    pub(crate) fn label(&self) -> &'static str {
        if self.is_daemon() { "daemon" } else { "direct" }
    }

    /// `exe` (an `mtseek`, or `rg` for comparisons) set up to run this way.
    pub(crate) fn command(&self, exe: &str) -> Command {
        let mut command = Command::new(exe);
        command.env("MTSEEK_CACHE_DIR", self.cache.path());
        #[cfg(windows)]
        if let Some(daemon) = &self.daemon {
            command.env("MTSEEK_PIPE", daemon.pipe()).arg("--daemon");
            return command;
        }
        command.env("MTSEEK_PIPE", NO_DAEMON);
        command
    }
}
