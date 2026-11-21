//! Local named pipes: the daemon's transport (ADR 0013).
//!
//! * The server creates the pipe with a DACL that admits only the user it runs as, refuses
//!   remote clients, and fails if the name is already taken.
//! * A client checks who runs the server before sending anything, so a process squatting on
//!   the name can't read its queries. A server can only identify a client, never act as it.
//!
//! Pipes are synchronous byte streams. A connection is a plain [`File`]: read requests
//! with a `BufReader`, write answers, and [`available`] says whether the peer sent something
//! without blocking.

use std::ffi::OsStr;
use std::fs::File;
use std::io;
use std::os::windows::io::{AsHandle, OwnedHandle};
use std::time::Duration;

use crate::sys;

/// The SID of the user this process runs as (`S-1-5-21-...`).
pub fn user_sid() -> io::Result<String> {
    sys::process_user_sid(None)
}

/// The SID of the service account a service-mode daemon runs as.
pub const LOCAL_SYSTEM: &str = "S-1-5-18";

/// The service's pipe (ADR 0014).
pub const SERVICE_PIPE: &str = r"\\.\pipe\mtseek";

/// The service pipe's DACL: network logons denied; SYSTEM, Administrators and the pipe's
/// owner (the process that created it) in full; interactive users may read and write
/// (`0x12019b`: generic read, write data, write attributes and extended attributes) but not
/// create instances of the pipe, so none can serve on it.
pub const SERVICE_PIPE_SDDL: &str =
    "D:P(D;;GA;;;NU)(A;;GA;;;SY)(A;;GA;;;BA)(A;;GA;;;OW)(A;;0x12019b;;;IU)";

/// The name of this user's console-mode daemon pipe: `\\.\pipe\mtseek-<SID>`.
pub fn user_pipe_name() -> io::Result<String> {
    Ok(format!(r"\\.\pipe\mtseek-{}", user_sid()?))
}

/// The server end of a pipe: accepts one client at a time.
pub struct Listener {
    name: String,
    descriptor: sys::SecurityDescriptor,
    /// The instance the next client connects to.
    next: OwnedHandle,
}

impl Listener {
    /// Creates pipe `name`, open only to the user this process runs as. Fails with
    /// [`io::ErrorKind::AlreadyExists`] if another process holds the name.
    pub fn bind(name: &str) -> io::Result<Self> {
        Self::bind_with(name, &format!("D:P(A;;GA;;;{})", user_sid()?))
    }

    /// Creates pipe `name` with the DACL in `sddl` (the service's: [`SERVICE_PIPE_SDDL`]).
    pub fn bind_with(name: &str, sddl: &str) -> io::Result<Self> {
        let descriptor = sys::security_descriptor(sddl)?;
        let next = sys::create_pipe(OsStr::new(name), true, &descriptor).map_err(|err| {
            // A first instance that exists already: access denied, by design of the flag.
            if err.kind() == io::ErrorKind::PermissionDenied {
                io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!("{name} is already in use (is another daemon running?)"),
                )
            } else {
                err
            }
        })?;
        Ok(Self {
            name: name.to_owned(),
            descriptor,
            next,
        })
    }

    /// The pipe's name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Waits for the next client and returns its connection. Another instance is ready for
    /// the client after it before this returns, so clients rarely find the pipe busy.
    pub fn accept(&mut self) -> io::Result<File> {
        sys::connect_pipe(self.next.as_handle())?;
        let after = sys::create_pipe(OsStr::new(&self.name), false, &self.descriptor)?;
        Ok(File::from(std::mem::replace(&mut self.next, after)))
    }
}

/// Connects to pipe `name`, waiting up to `timeout` if every instance is busy, and checks
/// that the server runs as `server_sid`. `Ok(None)` if no server listens on the name.
/// [`io::ErrorKind::PermissionDenied`] if the server runs as someone else.
pub fn connect(name: &str, timeout: Duration, server_sid: &str) -> io::Result<Option<File>> {
    let name_os = OsStr::new(name);
    let handle = match sys::open_pipe(name_os) {
        Ok(handle) => handle,
        Err(err) if is_busy(&err) => {
            let ms = u32::try_from(timeout.as_millis()).unwrap_or(u32::MAX);
            if !sys::wait_pipe(name_os, ms) {
                return Ok(None);
            }
            match sys::open_pipe(name_os) {
                Ok(handle) => handle,
                Err(err) if is_absent(&err) || is_busy(&err) => return Ok(None),
                Err(err) => return Err(err),
            }
        }
        Err(err) if is_absent(&err) => return Ok(None),
        Err(err) => return Err(err),
    };
    let pid = sys::pipe_server_pid(handle.as_handle())?;
    let owner = sys::process_user_sid(Some(pid)).map_err(|err| {
        io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("can't check who runs the server of {name}: {err}"),
        )
    })?;
    if owner != server_sid {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("the server of {name} runs as {owner}, not {server_sid}"),
        ));
    }
    Ok(Some(File::from(handle)))
}

/// How many bytes the peer has sent that can be read without blocking. A closed peer is
/// an error ([`io::ErrorKind::BrokenPipe`]).
pub fn available(pipe: &File) -> io::Result<usize> {
    Ok(sys::pipe_available(pipe.as_handle())? as usize)
}

fn is_busy(err: &io::Error) -> bool {
    // ERROR_PIPE_BUSY
    err.raw_os_error() == Some(231)
}

fn is_absent(err: &io::Error) -> bool {
    err.kind() == io::ErrorKind::NotFound
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::time::Instant;

    fn unique_name(test: &str) -> String {
        format!(r"\\.\pipe\mtseek-test-{test}-{}", std::process::id())
    }

    #[test]
    fn a_client_talks_to_the_server_both_ways() {
        let name = unique_name("talk");
        let mut listener = Listener::bind(&name).unwrap();
        let server = std::thread::spawn(move || {
            for _ in 0..2 {
                let conn = listener.accept().unwrap();
                let mut line = String::new();
                BufReader::new(&conn).read_line(&mut line).unwrap();
                (&conn).write_all(line.to_uppercase().as_bytes()).unwrap();
            }
        });
        let sid = user_sid().unwrap();
        for _ in 0..2 {
            let conn = connect(&name, Duration::from_millis(500), &sid)
                .unwrap()
                .expect("the server listens");
            (&conn).write_all(b"hello\n").unwrap();
            let mut answer = String::new();
            BufReader::new(&conn).read_line(&mut answer).unwrap();
            assert_eq!(answer, "HELLO\n");
        }
        server.join().unwrap();
    }

    #[test]
    fn no_server_is_none_at_once() {
        let started = Instant::now();
        let found = connect(
            &unique_name("nobody"),
            Duration::from_millis(500),
            &user_sid().unwrap(),
        )
        .unwrap();
        assert!(found.is_none());
        assert!(started.elapsed() < Duration::from_millis(400));
    }

    #[test]
    fn a_taken_name_is_refused() {
        let name = unique_name("taken");
        let _first = Listener::bind(&name).unwrap();
        let second = Listener::bind(&name).err().expect("the name is taken");
        assert_eq!(second.kind(), io::ErrorKind::AlreadyExists, "{second}");
    }

    #[test]
    fn a_server_running_as_someone_else_is_refused() {
        let name = unique_name("identity");
        let mut listener = Listener::bind(&name).unwrap();
        let server = std::thread::spawn(move || drop(listener.accept()));
        let err = connect(&name, Duration::from_millis(500), LOCAL_SYSTEM)
            .expect_err("this test doesn't run as LocalSystem");
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied, "{err}");
        server.join().unwrap();
    }

    #[test]
    fn available_counts_unread_bytes_and_a_closed_peer_reads_as_the_end() {
        let name = unique_name("peek");
        let mut listener = Listener::bind(&name).unwrap();
        let sid = user_sid().unwrap();
        let client = std::thread::spawn(move || {
            let conn = connect(&name, Duration::from_millis(500), &sid)
                .unwrap()
                .unwrap();
            (&conn).write_all(b"abc").unwrap();
            conn.sync_all().unwrap(); // waits until the server has read it
        });
        let mut conn = listener.accept().unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while available(&conn).unwrap() < 3 {
            assert!(Instant::now() < deadline);
            std::thread::yield_now();
        }
        let mut all = Vec::new();
        conn.read_to_end(&mut all).unwrap();
        assert_eq!(all, b"abc");
        client.join().unwrap();
        assert!(available(&conn).is_err(), "the client is gone");
    }

    #[test]
    fn the_user_pipe_name_carries_the_users_sid() {
        let name = user_pipe_name().unwrap();
        assert!(name.starts_with(r"\\.\pipe\mtseek-S-1-5-"), "{name}");
    }
}
