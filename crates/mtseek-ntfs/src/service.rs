//! Running as a Windows service, and the service control manager (ADR 0014).
//!
//! * [`install`] and [`uninstall`] register and remove a `LocalSystem` service (they need an
//!   elevated administrator).
//! * [`run`] is the service process's entry point: it hands the process to the SCM, runs
//!   the body, and tells the body when the SCM asks it to stop.
//! * [`secure_dir`] creates the machine-wide data folder, open only to SYSTEM and
//!   Administrators, and repairs one that someone else created first.

use std::ffi::OsStr;
use std::io;
use std::path::Path;
use std::sync::{Condvar, Mutex, OnceLock, PoisonError};
use std::time::{Duration, Instant};

use crate::sys;

const SC_MANAGER_CONNECT: u32 = 0x1;
const SC_MANAGER_CREATE_SERVICE: u32 = 0x2;
const SERVICE_ALL_ACCESS: u32 = 0xF01FF;
const SERVICE_QUERY_STATUS: u32 = 0x4;
const SERVICE_STOP: u32 = 0x20;
const DELETE: u32 = 0x1_0000;
const SERVICE_CONTROL_STOP: u32 = 1;
const STOPPED: u32 = 1;
const START_PENDING: u32 = 2;
const STOP_PENDING: u32 = 3;
const RUNNING: u32 = 4;
/// `ERROR_SERVICE_NOT_ACTIVE`: stopping a service that isn't running.
const NOT_ACTIVE: i32 = 1062;
/// `ERROR_SERVICE_SPECIFIC_ERROR`-like exit code reported when the body fails.
const BODY_FAILED: u32 = 1064;

/// The SDDL of the machine-wide data folder: owned by Administrators, full control for
/// SYSTEM and Administrators only, inherited by everything inside, nothing inherited from
/// above.
pub const MACHINE_DIR_SDDL: &str = "O:BAD:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)";

/// Registers service `name` (own process, `LocalSystem`, automatic start, restarted twice
/// after a failure) that runs `command`, and starts it if `start` is set.
pub fn install(
    name: &str,
    display: &str,
    description: &str,
    command: &str,
    start: bool,
) -> io::Result<()> {
    let manager = sys::sc_manager(SC_MANAGER_CONNECT | SC_MANAGER_CREATE_SERVICE)?;
    let service = sys::create_service(
        &manager,
        OsStr::new(name),
        OsStr::new(display),
        OsStr::new(command),
        SERVICE_ALL_ACCESS,
    )?;
    sys::configure_service(&service, OsStr::new(description), 2, 5_000)?;
    if start {
        sys::start_service(&service)?;
    }
    Ok(())
}

/// Stops service `name` (waiting up to `wait`) and deletes it.
pub fn uninstall(name: &str, wait: Duration) -> io::Result<()> {
    let manager = sys::sc_manager(SC_MANAGER_CONNECT)?;
    let service = sys::open_service(
        &manager,
        OsStr::new(name),
        SERVICE_STOP | SERVICE_QUERY_STATUS | DELETE,
    )?;
    match sys::control_service(&service, SERVICE_CONTROL_STOP) {
        Ok(_) => {}
        Err(e) if e.raw_os_error() == Some(NOT_ACTIVE) => {}
        Err(e) => return Err(e),
    }
    let deadline = Instant::now() + wait;
    while sys::service_state(&service)? != STOPPED {
        if Instant::now() > deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("{name} did not stop within {} s", wait.as_secs()),
            ));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    sys::delete_service(&service)
}

/// Whether service `name` exists and is running.
pub fn is_running(name: &str) -> io::Result<bool> {
    let manager = sys::sc_manager(SC_MANAGER_CONNECT)?;
    let service = sys::open_service(&manager, OsStr::new(name), SERVICE_QUERY_STATUS)?;
    Ok(sys::service_state(&service)? == RUNNING)
}

/// What a service body is told by [`run`].
pub struct Service {
    status: sys::ServiceStatus,
}

impl Service {
    /// Tells the SCM the service is up. Call it once ready to serve.
    pub fn running(&self) {
        self.status.set(RUNNING, 0, 0);
    }

    /// Blocks until the SCM asks the service to stop (stop, shutdown or preshutdown).
    pub fn wait_for_stop(&self) {
        let (lock, wake) = &STOP;
        let mut stop = lock.lock().unwrap_or_else(PoisonError::into_inner);
        while !*stop {
            stop = wake.wait(stop).unwrap_or_else(PoisonError::into_inner);
        }
    }

    /// Like [`Service::wait_for_stop`], but gives up after `timeout`. Returns whether the
    /// SCM asked the service to stop.
    pub fn wait_for_stop_timeout(&self, timeout: Duration) -> bool {
        let (lock, wake) = &STOP;
        let stop = lock.lock().unwrap_or_else(PoisonError::into_inner);
        let (stop, _) = wake
            .wait_timeout_while(stop, timeout, |stop| !*stop)
            .unwrap_or_else(PoisonError::into_inner);
        *stop
    }
}

type Body = Box<dyn Fn(&Service) -> Result<(), String> + Send + Sync>;

static BODY: OnceLock<Body> = OnceLock::new();
static STATUS: OnceLock<sys::ServiceStatus> = OnceLock::new();
static STOP: (Mutex<bool>, Condvar) = (Mutex::new(false), Condvar::new());

fn on_stop() {
    let (lock, wake) = &STOP;
    *lock.lock().unwrap_or_else(PoisonError::into_inner) = true;
    wake.notify_all();
    if let Some(status) = STATUS.get() {
        status.set(STOP_PENDING, 0, 30_000);
    }
}

fn service_body(status: sys::ServiceStatus) {
    let _ = STATUS.set(status);
    status.set(START_PENDING, 0, 30_000);
    let result = BODY.get().map_or(Ok(()), |body| body(&Service { status }));
    status.set(STOPPED, if result.is_ok() { 0 } else { BODY_FAILED }, 0);
}

/// Runs `body` as this process's service and returns when it has stopped. The body calls
/// [`Service::running`] once it serves and [`Service::wait_for_stop`] to learn when to
/// stop. Fails with error 1063 if the process wasn't started by the SCM.
pub fn run(
    body: impl Fn(&Service) -> Result<(), String> + Send + Sync + 'static,
) -> io::Result<()> {
    let _ = BODY.set(Box::new(body));
    sys::service_dispatch(service_body, on_stop)
}

/// Creates folder `path` for machine-wide data with [`MACHINE_DIR_SDDL`], or makes an
/// existing one safe: one that is a link or junction, or that isn't owned by SYSTEM or
/// Administrators (anyone may create folders in `%ProgramData%`), is deleted and created
/// again; one that is, gets the DACL back.
pub fn secure_dir(path: &Path) -> io::Result<()> {
    let descriptor = sys::security_descriptor(MACHINE_DIR_SDDL)?;
    match std::fs::symlink_metadata(path) {
        Ok(meta) => {
            use std::os::windows::fs::MetadataExt;
            const REPARSE_POINT: u32 = 0x400;
            if meta.file_attributes() & REPARSE_POINT != 0 || !meta.is_dir() {
                if meta.is_dir() {
                    std::fs::remove_dir(path)?;
                } else {
                    std::fs::remove_file(path)?;
                }
            } else {
                let owner = sys::path_owner(path.as_os_str())?;
                if owner == "S-1-5-18" || owner == "S-1-5-32-544" {
                    return sys::set_path_security(path.as_os_str(), &descriptor, true);
                }
                std::fs::remove_dir_all(path)?;
            }
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    sys::create_directory(path.as_os_str(), &descriptor)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_process_the_scm_did_not_start_is_told_so() {
        let err = run(|_| Ok(())).expect_err("tests aren't services");
        assert_eq!(err.raw_os_error(), Some(1063), "{err}");
    }

    #[test]
    fn the_machine_folder_descriptor_is_well_formed() {
        let d = crate::security::descriptor_from_sddl(MACHINE_DIR_SDDL).unwrap();
        assert!(crate::security::well_formed(&d));
    }
}
