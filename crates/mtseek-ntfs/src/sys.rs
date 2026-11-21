//! Raw Win32 calls: the only module in mtseek that contains `unsafe`.
//!
//! Each function wraps one API (or one short, fixed sequence of them), checks its result,
//! and hands back safe Rust types (`OwnedHandle`, byte counts, strings, `io::Error`). Keep it
//! that small. Besides NTFS, it holds the named-pipe and token calls of the daemon's
//! transport (ADR 0013), the only other Win32 code that needs `unsafe`.

#![allow(unsafe_code)]

use std::ffi::OsStr;
use std::io;
use std::mem::MaybeUninit;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, BorrowedHandle, FromRawHandle, OwnedHandle};
use std::ptr;

use windows_sys::Win32::Foundation::{
    ERROR_NO_MORE_FILES, ERROR_PIPE_CONNECTED, GENERIC_READ, GENERIC_WRITE, INVALID_HANDLE_VALUE,
    LocalFree,
};
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows_sys::Win32::Security::{
    GetTokenInformation, PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER,
    TokenUser,
};
use windows_sys::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, CreateFileW, FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_SHARE_DELETE,
    FILE_SHARE_READ, FILE_SHARE_WRITE, FileIdBothDirectoryInfo, FileIdBothDirectoryRestartInfo,
    GetFileInformationByHandle, GetFileInformationByHandleEx, OPEN_EXISTING, PIPE_ACCESS_DUPLEX,
    SECURITY_IDENTIFICATION, SECURITY_SQOS_PRESENT,
};
use windows_sys::Win32::System::IO::DeviceIoControl;
use windows_sys::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, GetNamedPipeServerProcessId, PIPE_READMODE_BYTE,
    PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES, PIPE_WAIT, PeekNamedPipe,
    WaitNamedPipeW,
};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows_sys::core::PWSTR;

/// `CreateFileW(path, access, share everything, OPEN_EXISTING, flags)`.
pub(crate) fn open(path: &OsStr, access: u32, flags: u32) -> io::Result<OwnedHandle> {
    let path: Vec<u16> = path.encode_wide().chain(Some(0)).collect();
    // SAFETY: `path` is a NUL-terminated UTF-16 string that lives until the call returns.
    // The security-attributes and template arguments may be null.
    let handle = unsafe {
        CreateFileW(
            path.as_ptr(),
            access,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            ptr::null(),
            OPEN_EXISTING,
            flags,
            ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `handle` is a valid handle that nothing else owns; `OwnedHandle` closes it once.
    Ok(unsafe { OwnedHandle::from_raw_handle(handle) })
}

/// Synchronous `DeviceIoControl`. Returns the number of bytes written to `output`.
pub(crate) fn ioctl(
    handle: BorrowedHandle<'_>,
    code: u32,
    input: &[u8],
    output: &mut [u8],
) -> io::Result<usize> {
    let mut returned = 0u32;
    let input_ptr = if input.is_empty() {
        ptr::null()
    } else {
        input.as_ptr().cast()
    };
    // SAFETY: the pointers and lengths describe live buffers borrowed for the duration of
    // the call. There is no OVERLAPPED, so the call completes before returning and the
    // kernel keeps no reference to either buffer.
    let ok = unsafe {
        DeviceIoControl(
            handle.as_raw_handle(),
            code,
            input_ptr,
            len_u32(input.len()),
            output.as_mut_ptr().cast(),
            len_u32(output.len()),
            &raw mut returned,
            ptr::null_mut(),
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(returned as usize)
}

/// The 64-bit file index (on NTFS: the file reference number) of an open handle.
pub(crate) fn file_index(handle: BorrowedHandle<'_>) -> io::Result<u64> {
    let mut info = MaybeUninit::<BY_HANDLE_FILE_INFORMATION>::zeroed();
    // SAFETY: `info` is a writable, correctly typed buffer and `handle` is open.
    let ok = unsafe { GetFileInformationByHandle(handle.as_raw_handle(), info.as_mut_ptr()) };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the call succeeded, which means it filled in `info`.
    let info = unsafe { info.assume_init() };
    Ok((u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow))
}

/// One `GetFileInformationByHandleEx(FileIdBothDirectory[Restart]Info)` call on a directory
/// handle. Returns the whole buffer as bytes (the records chain from offset 0), or `None`
/// once the directory has no more entries.
pub(crate) fn query_directory<'a>(
    handle: &OwnedHandle,
    buf: &'a mut [u64],
    restart: bool,
) -> io::Result<Option<&'a [u8]>> {
    let class = if restart {
        FileIdBothDirectoryRestartInfo
    } else {
        FileIdBothDirectoryInfo
    };
    let bytes = size_of_val(buf);
    // SAFETY: `buf` is writable, 8-byte aligned (as FILE_ID_BOTH_DIR_INFO requires) and
    // `bytes` long. The handle is synchronous, so the call is finished when it returns.
    let ok = unsafe {
        GetFileInformationByHandleEx(
            handle.as_raw_handle(),
            class,
            buf.as_mut_ptr().cast(),
            len_u32(bytes),
        )
    };
    if ok == 0 {
        let err = io::Error::last_os_error();
        if err.raw_os_error() == i32::try_from(ERROR_NO_MORE_FILES).ok() {
            return Ok(None);
        }
        return Err(err);
    }
    // SAFETY: `u8` has no alignment or validity requirements, and the slice covers exactly
    // the memory of `buf`, which stays borrowed for the slice's lifetime.
    Ok(Some(unsafe {
        std::slice::from_raw_parts(buf.as_ptr().cast::<u8>(), bytes)
    }))
}

/// A self-relative security descriptor made from SDDL, freed on drop.
pub(crate) struct SecurityDescriptor(PSECURITY_DESCRIPTOR);

// SAFETY: the descriptor is plain memory that only `LocalFree` (in `drop`) releases, and
// the Win32 calls that take it only read it.
unsafe impl Send for SecurityDescriptor {}
// SAFETY: as above; shared references only hand the pointer to read-only Win32 calls.
unsafe impl Sync for SecurityDescriptor {}

impl Drop for SecurityDescriptor {
    fn drop(&mut self) {
        // SAFETY: the pointer came from `ConvertStringSecurityDescriptorToSecurityDescriptorW`,
        // which allocates with `LocalAlloc`, and is freed exactly once.
        unsafe { LocalFree(self.0) };
    }
}

/// `ConvertStringSecurityDescriptorToSecurityDescriptorW(sddl)`.
pub(crate) fn security_descriptor(sddl: &str) -> io::Result<SecurityDescriptor> {
    let sddl: Vec<u16> = sddl.encode_utf16().chain(Some(0)).collect();
    let mut descriptor: PSECURITY_DESCRIPTOR = ptr::null_mut();
    // SAFETY: `sddl` is NUL-terminated and outlives the call; `descriptor` is a valid out
    // pointer, and the size out pointer may be null.
    let ok = unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            SDDL_REVISION_1,
            &raw mut descriptor,
            ptr::null_mut(),
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(SecurityDescriptor(descriptor))
}

/// `CreateNamedPipeW`: a duplex, byte-mode, blocking instance of pipe `name` that refuses
/// remote clients, protected by `descriptor`. `first` fails if the pipe already exists.
pub(crate) fn create_pipe(
    name: &OsStr,
    first: bool,
    descriptor: &SecurityDescriptor,
) -> io::Result<OwnedHandle> {
    const BUFFER: u32 = 64 * 1024;
    let name: Vec<u16> = name.encode_wide().chain(Some(0)).collect();
    let attributes = SECURITY_ATTRIBUTES {
        nLength: len_u32(size_of::<SECURITY_ATTRIBUTES>()),
        lpSecurityDescriptor: descriptor.0,
        bInheritHandle: 0,
    };
    let open_mode = PIPE_ACCESS_DUPLEX
        | if first {
            FILE_FLAG_FIRST_PIPE_INSTANCE
        } else {
            0
        };
    // SAFETY: `name` is NUL-terminated, and `attributes` and the descriptor it points to
    // outlive the call.
    let handle = unsafe {
        CreateNamedPipeW(
            name.as_ptr(),
            open_mode,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
            PIPE_UNLIMITED_INSTANCES,
            BUFFER,
            BUFFER,
            0,
            &raw const attributes,
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `handle` is a valid handle that nothing else owns.
    Ok(unsafe { OwnedHandle::from_raw_handle(handle) })
}

/// `ConnectNamedPipe`: waits for a client on a server instance. A client that connected
/// before the call counts.
pub(crate) fn connect_pipe(handle: BorrowedHandle<'_>) -> io::Result<()> {
    // SAFETY: the handle is a synchronous pipe instance, so no OVERLAPPED is needed.
    let ok = unsafe { ConnectNamedPipe(handle.as_raw_handle(), ptr::null_mut()) };
    if ok == 0 {
        let err = io::Error::last_os_error();
        if err.raw_os_error() != i32::try_from(ERROR_PIPE_CONNECTED).ok() {
            return Err(err);
        }
    }
    Ok(())
}

/// Opens the client end of pipe `name` for reading and writing. The server may only
/// identify the client, never act as it (`SECURITY_IDENTIFICATION`).
pub(crate) fn open_pipe(name: &OsStr) -> io::Result<OwnedHandle> {
    let name: Vec<u16> = name.encode_wide().chain(Some(0)).collect();
    // SAFETY: `name` is NUL-terminated and lives until the call returns. The
    // security-attributes and template arguments may be null.
    let handle = unsafe {
        CreateFileW(
            name.as_ptr(),
            GENERIC_READ | GENERIC_WRITE,
            0,
            ptr::null(),
            OPEN_EXISTING,
            SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION,
            ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `handle` is a valid handle that nothing else owns.
    Ok(unsafe { OwnedHandle::from_raw_handle(handle) })
}

/// `WaitNamedPipeW`: waits up to `ms` for an instance of pipe `name` to be free.
pub(crate) fn wait_pipe(name: &OsStr, ms: u32) -> bool {
    let name: Vec<u16> = name.encode_wide().chain(Some(0)).collect();
    // SAFETY: `name` is NUL-terminated and lives until the call returns.
    unsafe { WaitNamedPipeW(name.as_ptr(), ms) != 0 }
}

/// `PeekNamedPipe`: how many bytes can be read from the pipe without blocking.
pub(crate) fn pipe_available(handle: BorrowedHandle<'_>) -> io::Result<u32> {
    let mut available = 0u32;
    // SAFETY: no buffer is passed (null, length 0); only the count is written.
    let ok = unsafe {
        PeekNamedPipe(
            handle.as_raw_handle(),
            ptr::null_mut(),
            0,
            ptr::null_mut(),
            &raw mut available,
            ptr::null_mut(),
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(available)
}

/// `GetNamedPipeServerProcessId`: the process id of the server end of a client pipe.
pub(crate) fn pipe_server_pid(handle: BorrowedHandle<'_>) -> io::Result<u32> {
    let mut pid = 0u32;
    // SAFETY: `pid` is a valid out pointer and the handle is open.
    let ok = unsafe { GetNamedPipeServerProcessId(handle.as_raw_handle(), &raw mut pid) };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(pid)
}

/// The user SID (`S-1-5-21-...`) of process `pid`, or of this process for `None`.
pub(crate) fn process_user_sid(pid: Option<u32>) -> io::Result<String> {
    let process = match pid {
        None => None,
        Some(pid) => {
            // SAFETY: plain call; the result is checked before use.
            let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
            if handle.is_null() {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: `handle` is a valid process handle that nothing else owns.
            Some(unsafe { OwnedHandle::from_raw_handle(handle) })
        }
    };
    let raw_process = match &process {
        Some(handle) => handle.as_raw_handle(),
        // SAFETY: `GetCurrentProcess` has no preconditions; its pseudo-handle needs no
        // closing.
        None => unsafe { GetCurrentProcess() },
    };
    let mut token = ptr::null_mut();
    // SAFETY: `raw_process` is a valid process handle (or the current-process
    // pseudo-handle) for the duration of the call; `token` is a valid out pointer.
    if unsafe { OpenProcessToken(raw_process, TOKEN_QUERY, &raw mut token) } == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `token` is a valid token handle that nothing else owns.
    let token = unsafe { OwnedHandle::from_raw_handle(token) };
    // TOKEN_USER is a pointer to the SID plus attributes; the SID follows in the buffer.
    let mut buf = vec![0u64; 64];
    let mut needed = 0u32;
    // SAFETY: `buf` is writable and 8-byte aligned, as TOKEN_USER requires; its length in
    // bytes is passed.
    let ok = unsafe {
        GetTokenInformation(
            token.as_raw_handle(),
            TokenUser,
            buf.as_mut_ptr().cast(),
            len_u32(size_of_val(buf.as_slice())),
            &raw mut needed,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the call succeeded, so the buffer starts with a TOKEN_USER whose SID pointer
    // points into the same buffer, which is still alive.
    let sid = unsafe { (*buf.as_ptr().cast::<TOKEN_USER>()).User.Sid };
    let mut text: PWSTR = ptr::null_mut();
    // SAFETY: `sid` is a valid SID (see above) and `text` a valid out pointer.
    if unsafe { ConvertSidToStringSidW(sid, &raw mut text) } == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: on success `text` is a NUL-terminated UTF-16 string allocated with
    // `LocalAlloc`. It is read up to the NUL, then freed once.
    let sid = unsafe {
        let mut len = 0;
        while *text.add(len) != 0 {
            len += 1;
        }
        let sid = String::from_utf16_lossy(std::slice::from_raw_parts(text, len));
        LocalFree(text.cast());
        sid
    };
    Ok(sid)
}

fn len_u32(len: usize) -> u32 {
    u32::try_from(len).unwrap_or(u32::MAX)
}
