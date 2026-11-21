//! Raw Win32 calls: the only module in mtseek that contains `unsafe`.
//!
//! Each function wraps one API (or one short, fixed sequence of them), checks its result,
//! and hands back safe Rust types (`OwnedHandle`, byte counts, strings, `io::Error`). Keep it
//! that small. Besides NTFS, it holds the named-pipe and token calls of the daemon's
//! transport (ADR 0013) and the security, access-check and service-control calls of service
//! mode (ADR 0014), the only other Win32 code that needs `unsafe`.

#![allow(unsafe_code)]

use std::ffi::{OsStr, c_void};
use std::io;
use std::mem::MaybeUninit;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsHandle, AsRawHandle, BorrowedHandle, FromRawHandle, OwnedHandle};
use std::ptr;
use std::sync::OnceLock;

use windows_sys::Win32::Foundation::{
    ERROR_CALL_NOT_IMPLEMENTED, ERROR_NO_MORE_FILES, ERROR_PIPE_CONNECTED, INVALID_HANDLE_VALUE,
    LocalFree, NO_ERROR,
};
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
    GetNamedSecurityInfoW, GetSecurityInfo, SDDL_REVISION_1, SE_FILE_OBJECT, SetNamedSecurityInfoW,
};
use windows_sys::Win32::Security::{
    ACL, AccessCheck, CheckTokenMembership, CreateWellKnownSid, DACL_SECURITY_INFORMATION,
    DuplicateToken, GENERIC_MAPPING, GROUP_SECURITY_INFORMATION, GetSecurityDescriptorDacl,
    GetSecurityDescriptorLength, GetSecurityDescriptorOwner, GetTokenInformation,
    LABEL_SECURITY_INFORMATION, OWNER_SECURITY_INFORMATION, PRIVILEGE_SET,
    PROTECTED_DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID, RevertToSelf,
    SECURITY_ATTRIBUTES, SecurityIdentification, TOKEN_DUPLICATE, TOKEN_QUERY, TOKEN_USER,
    TokenUser, WinBuiltinAdministratorsSid,
};
use windows_sys::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, CreateDirectoryW, CreateFileW, FILE_ALL_ACCESS,
    FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_GENERIC_EXECUTE, FILE_GENERIC_READ, FILE_GENERIC_WRITE,
    FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_WRITE_ATTRIBUTES, FILE_WRITE_DATA,
    FileIdBothDirectoryInfo, FileIdBothDirectoryRestartInfo, GetFileInformationByHandle,
    GetFileInformationByHandleEx, OPEN_EXISTING, PIPE_ACCESS_DUPLEX, SECURITY_IDENTIFICATION,
    SECURITY_SQOS_PRESENT,
};
use windows_sys::Win32::System::IO::DeviceIoControl;
use windows_sys::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, GetNamedPipeServerProcessId, ImpersonateNamedPipeClient,
    PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES,
    PIPE_WAIT, PeekNamedPipe, WaitNamedPipeW,
};
use windows_sys::Win32::System::Services::{
    ChangeServiceConfig2W, CloseServiceHandle, ControlService, CreateServiceW, DeleteService,
    OpenSCManagerW, OpenServiceW, QueryServiceStatus, RegisterServiceCtrlHandlerExW, SC_ACTION,
    SC_ACTION_RESTART, SC_HANDLE, SERVICE_ACCEPT_PRESHUTDOWN, SERVICE_ACCEPT_SHUTDOWN,
    SERVICE_ACCEPT_STOP, SERVICE_AUTO_START, SERVICE_CONFIG_DESCRIPTION,
    SERVICE_CONFIG_FAILURE_ACTIONS, SERVICE_CONTROL_INTERROGATE, SERVICE_CONTROL_PRESHUTDOWN,
    SERVICE_CONTROL_SHUTDOWN, SERVICE_CONTROL_STOP, SERVICE_DESCRIPTIONW, SERVICE_ERROR_NORMAL,
    SERVICE_FAILURE_ACTIONSW, SERVICE_RUNNING, SERVICE_STATUS, SERVICE_STATUS_HANDLE,
    SERVICE_TABLE_ENTRYW, SERVICE_WIN32_OWN_PROCESS, SetServiceStatus, StartServiceCtrlDispatcherW,
    StartServiceW,
};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, GetCurrentThread, OpenProcess, OpenProcessToken, OpenThreadToken,
    PROCESS_QUERY_LIMITED_INFORMATION,
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
    file_identity(handle).map(|(_, index)| index)
}

/// The volume serial number and 64-bit file index of an open handle: together they name
/// one file on the machine.
pub(crate) fn file_identity(handle: BorrowedHandle<'_>) -> io::Result<(u32, u64)> {
    let mut info = MaybeUninit::<BY_HANDLE_FILE_INFORMATION>::zeroed();
    // SAFETY: `info` is a writable, correctly typed buffer and `handle` is open.
    let ok = unsafe { GetFileInformationByHandle(handle.as_raw_handle(), info.as_mut_ptr()) };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the call succeeded, which means it filled in `info`.
    let info = unsafe { info.assume_init() };
    Ok((
        info.dwVolumeSerialNumber,
        (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow),
    ))
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

impl SecurityDescriptor {
    /// The descriptor as self-relative bytes.
    pub(crate) fn to_bytes(&self) -> Vec<u8> {
        // SAFETY: the pointer is a valid self-relative descriptor that lives as long as
        // `self`.
        unsafe { descriptor_bytes(self.0) }
    }
}

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

/// What a client asks of a pipe: read, write data and attributes, but not
/// `FILE_CREATE_PIPE_INSTANCE`, which the service's pipe grants nobody but SYSTEM and
/// Administrators (ADR 0014).
const CLIENT_ACCESS: u32 = FILE_GENERIC_READ | FILE_WRITE_DATA | FILE_WRITE_ATTRIBUTES;

/// Opens the client end of pipe `name` for reading and writing. The server may only
/// identify the client, never act as it (`SECURITY_IDENTIFICATION`).
pub(crate) fn open_pipe(name: &OsStr) -> io::Result<OwnedHandle> {
    let name: Vec<u16> = name.encode_wide().chain(Some(0)).collect();
    // SAFETY: `name` is NUL-terminated and lives until the call returns. The
    // security-attributes and template arguments may be null.
    let handle = unsafe {
        CreateFileW(
            name.as_ptr(),
            CLIENT_ACCESS,
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
    let token = process_token(pid, TOKEN_QUERY)?;
    token_user_sid(token.as_handle())
}

/// The token of process `pid` (or this process), opened for `access`.
fn process_token(pid: Option<u32>, access: u32) -> io::Result<OwnedHandle> {
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
    if unsafe { OpenProcessToken(raw_process, access, &raw mut token) } == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `token` is a valid token handle that nothing else owns.
    Ok(unsafe { OwnedHandle::from_raw_handle(token) })
}

/// The user SID of a token, as text.
pub(crate) fn token_user_sid(token: BorrowedHandle<'_>) -> io::Result<String> {
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
    // SAFETY: as above, `sid` is valid while `buf` lives.
    unsafe { sid_text(sid) }
}

/// A SID as text (`S-1-5-...`).
///
/// # Safety
///
/// `sid` must point to a valid SID.
unsafe fn sid_text(sid: PSID) -> io::Result<String> {
    let mut text: PWSTR = ptr::null_mut();
    // SAFETY: `sid` is valid (the caller's promise) and `text` a valid out pointer.
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

// ---------------------------------------------------------------------------------------
// Security descriptors, tokens and access checks (ADR 0014)

/// What is recorded of a file's descriptor: owner, group, DACL and mandatory label, which
/// is everything `AccessCheck` needs for a read or list decision.
const RECORDED: u32 = OWNER_SECURITY_INFORMATION
    | GROUP_SECURITY_INFORMATION
    | DACL_SECURITY_INFORMATION
    | LABEL_SECURITY_INFORMATION;

/// Copies a self-relative descriptor into bytes.
///
/// # Safety
///
/// `sd` must point to a valid self-relative security descriptor.
unsafe fn descriptor_bytes(sd: PSECURITY_DESCRIPTOR) -> Vec<u8> {
    // SAFETY: `sd` is valid (the caller's promise), so its length is readable and that many
    // bytes are readable at `sd`.
    unsafe {
        let len = GetSecurityDescriptorLength(sd) as usize;
        std::slice::from_raw_parts(sd.cast::<u8>(), len).to_vec()
    }
}

/// `GetSecurityInfo` on an open file or directory handle (which needs `READ_CONTROL`).
pub(crate) fn handle_descriptor(handle: BorrowedHandle<'_>) -> io::Result<Vec<u8>> {
    let mut sd: PSECURITY_DESCRIPTOR = ptr::null_mut();
    // SAFETY: the handle is open; the owner, group, DACL and SACL out pointers may be null,
    // and `sd` is a valid out pointer.
    let rc = unsafe {
        GetSecurityInfo(
            handle.as_raw_handle(),
            SE_FILE_OBJECT,
            RECORDED,
            ptr::null_mut(),
            ptr::null_mut(),
            ptr::null_mut(),
            ptr::null_mut(),
            &raw mut sd,
        )
    };
    if rc != NO_ERROR {
        return Err(io::Error::from_raw_os_error(code(rc)));
    }
    // SAFETY: on success `sd` is a self-relative descriptor allocated with `LocalAlloc`; it
    // is copied, then freed once.
    unsafe {
        let bytes = descriptor_bytes(sd);
        LocalFree(sd);
        Ok(bytes)
    }
}

/// `SetNamedSecurityInfoW` with the DACL of `descriptor`, protected from inheritance, and
/// its owner too if `owner` is set.
pub(crate) fn set_path_security(
    path: &OsStr,
    descriptor: &SecurityDescriptor,
    owner: bool,
) -> io::Result<()> {
    let path = wide(path);
    let mut present = 0;
    let mut defaulted = 0;
    let mut dacl: *mut ACL = ptr::null_mut();
    let mut owner_sid: PSID = ptr::null_mut();
    // SAFETY: the descriptor is valid while `descriptor` lives; the out pointers are valid.
    // The DACL and owner pointers point into the descriptor.
    unsafe {
        if GetSecurityDescriptorDacl(
            descriptor.0,
            &raw mut present,
            &raw mut dacl,
            &raw mut defaulted,
        ) == 0
        {
            return Err(io::Error::last_os_error());
        }
        if owner
            && GetSecurityDescriptorOwner(descriptor.0, &raw mut owner_sid, &raw mut defaulted) == 0
        {
            return Err(io::Error::last_os_error());
        }
    }
    let mut info = DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION;
    if owner {
        info |= OWNER_SECURITY_INFORMATION;
    }
    // SAFETY: `path` is NUL-terminated; the owner and DACL point into `descriptor`, which
    // outlives the call; the group and SACL may be null.
    let rc = unsafe {
        SetNamedSecurityInfoW(
            path.as_ptr(),
            SE_FILE_OBJECT,
            info,
            owner_sid,
            ptr::null_mut(),
            dacl,
            ptr::null(),
        )
    };
    if rc != NO_ERROR {
        return Err(io::Error::from_raw_os_error(code(rc)));
    }
    Ok(())
}

/// The owner SID of the file or directory at `path`, as text.
pub(crate) fn path_owner(path: &OsStr) -> io::Result<String> {
    let path = wide(path);
    let mut owner: PSID = ptr::null_mut();
    let mut sd: PSECURITY_DESCRIPTOR = ptr::null_mut();
    // SAFETY: `path` is NUL-terminated; `owner` and `sd` are valid out pointers, the rest
    // may be null.
    let rc = unsafe {
        GetNamedSecurityInfoW(
            path.as_ptr(),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION,
            &raw mut owner,
            ptr::null_mut(),
            ptr::null_mut(),
            ptr::null_mut(),
            &raw mut sd,
        )
    };
    if rc != NO_ERROR {
        return Err(io::Error::from_raw_os_error(code(rc)));
    }
    // SAFETY: `owner` points into `sd`, which is freed once after reading it.
    unsafe {
        let text = sid_text(owner);
        LocalFree(sd);
        text
    }
}

/// `CreateDirectoryW` with `descriptor` as the new directory's security.
pub(crate) fn create_directory(path: &OsStr, descriptor: &SecurityDescriptor) -> io::Result<()> {
    let path = wide(path);
    let attributes = SECURITY_ATTRIBUTES {
        nLength: len_u32(size_of::<SECURITY_ATTRIBUTES>()),
        lpSecurityDescriptor: descriptor.0,
        bInheritHandle: 0,
    };
    // SAFETY: `path` is NUL-terminated; `attributes` and its descriptor outlive the call.
    if unsafe { CreateDirectoryW(path.as_ptr(), &raw const attributes) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// An identification-level token of this process: good for access checks, nothing else.
pub(crate) fn own_identification_token() -> io::Result<OwnedHandle> {
    let process = process_token(None, TOKEN_DUPLICATE | TOKEN_QUERY)?;
    let mut token = ptr::null_mut();
    // SAFETY: `process` is an open token with TOKEN_DUPLICATE; `token` is a valid out
    // pointer.
    if unsafe {
        DuplicateToken(
            process.as_raw_handle(),
            SecurityIdentification,
            &raw mut token,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `token` is a valid token handle that nothing else owns.
    Ok(unsafe { OwnedHandle::from_raw_handle(token) })
}

/// The token of the client of a server pipe instance, at the level the client allowed
/// (identification for mtseek's clients). The pipe must have been read from first.
/// Impersonation lasts only for `OpenThreadToken`; the thread reverts before returning.
pub(crate) fn pipe_client_token(pipe: BorrowedHandle<'_>) -> io::Result<OwnedHandle> {
    // SAFETY: the handle is a connected server pipe instance.
    if unsafe { ImpersonateNamedPipeClient(pipe.as_raw_handle()) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let mut token = ptr::null_mut();
    // SAFETY: the current-thread pseudo-handle needs no closing; `token` is a valid out
    // pointer. `OpenAsSelf` makes the open use the process's own identity.
    let opened = unsafe { OpenThreadToken(GetCurrentThread(), TOKEN_QUERY, 1, &raw mut token) };
    let err = io::Error::last_os_error();
    // SAFETY: plain call. Failing to revert would leave the thread running as the client,
    // so it is fatal.
    if unsafe { RevertToSelf() } == 0 {
        std::process::abort();
    }
    if opened == 0 {
        return Err(err);
    }
    // SAFETY: `token` is a valid token handle that nothing else owns.
    Ok(unsafe { OwnedHandle::from_raw_handle(token) })
}

/// `AccessCheck`: whether `token` is granted `desired` (generic rights mapped as for files)
/// by the self-relative `descriptor`. The caller checks the descriptor's structure first
/// (`security::well_formed`), so that every offset in it stays inside it.
pub(crate) fn access_check(
    token: BorrowedHandle<'_>,
    descriptor: &[u8],
    desired: u32,
) -> io::Result<bool> {
    let mapping = GENERIC_MAPPING {
        GenericRead: FILE_GENERIC_READ,
        GenericWrite: FILE_GENERIC_WRITE,
        GenericExecute: FILE_GENERIC_EXECUTE,
        GenericAll: FILE_ALL_ACCESS,
    };
    // A descriptor is read through 4-byte fields; copy it into aligned memory.
    let mut aligned = vec![0u64; descriptor.len().div_ceil(8)];
    // SAFETY: `aligned` has room for `descriptor.len()` bytes and doesn't overlap it.
    unsafe {
        ptr::copy_nonoverlapping(
            descriptor.as_ptr(),
            aligned.as_mut_ptr().cast::<u8>(),
            descriptor.len(),
        );
    }
    let mut privileges = [0u64; 8];
    let mut privileges_len = len_u32(size_of_val(&privileges));
    let mut granted = 0u32;
    let mut status = 0;
    // SAFETY: the descriptor's offsets were checked against its length by the caller, so
    // every read stays inside `aligned`; `token` is an impersonation token; the out pointers
    // are valid and `privileges` is as big as `privileges_len` says.
    let ok = unsafe {
        AccessCheck(
            aligned.as_mut_ptr().cast(),
            token.as_raw_handle(),
            desired,
            &raw const mapping,
            privileges.as_mut_ptr().cast::<PRIVILEGE_SET>(),
            &raw mut privileges_len,
            &raw mut granted,
            &raw mut status,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(status != 0)
}

/// Whether `token` holds the Administrators group enabled: an elevated administrator.
pub(crate) fn token_is_admin(token: BorrowedHandle<'_>) -> io::Result<bool> {
    let mut sid = [0u64; 12];
    let mut len = len_u32(size_of_val(&sid));
    // SAFETY: `sid` is writable and `len` bytes long; the domain SID may be null.
    if unsafe {
        CreateWellKnownSid(
            WinBuiltinAdministratorsSid,
            ptr::null_mut(),
            sid.as_mut_ptr().cast(),
            &raw mut len,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    let mut member = 0;
    // SAFETY: `token` is an impersonation token, `sid` a valid SID, `member` a valid out
    // pointer.
    if unsafe {
        CheckTokenMembership(
            token.as_raw_handle(),
            sid.as_mut_ptr().cast(),
            &raw mut member,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(member != 0)
}

// ---------------------------------------------------------------------------------------
// The service control manager (ADR 0014)

/// A service control manager or service handle, closed on drop.
pub(crate) struct ScHandle(SC_HANDLE);

impl Drop for ScHandle {
    fn drop(&mut self) {
        // SAFETY: the handle came from `OpenSCManagerW`, `OpenServiceW` or `CreateServiceW`
        // and is closed once.
        unsafe { CloseServiceHandle(self.0) };
    }
}

fn sc_result(handle: SC_HANDLE) -> io::Result<ScHandle> {
    if handle.is_null() {
        Err(io::Error::last_os_error())
    } else {
        Ok(ScHandle(handle))
    }
}

fn wide(s: &OsStr) -> Vec<u16> {
    s.encode_wide().chain(Some(0)).collect()
}

/// `OpenSCManagerW` on this machine.
pub(crate) fn sc_manager(access: u32) -> io::Result<ScHandle> {
    // SAFETY: null machine and database names mean this machine's active database.
    sc_result(unsafe { OpenSCManagerW(ptr::null(), ptr::null(), access) })
}

/// `CreateServiceW`: an auto-start service in its own process, running as `LocalSystem`.
pub(crate) fn create_service(
    manager: &ScHandle,
    name: &OsStr,
    display: &OsStr,
    command: &OsStr,
    access: u32,
) -> io::Result<ScHandle> {
    let (name, display, command) = (wide(name), wide(display), wide(command));
    // SAFETY: the strings are NUL-terminated and outlive the call; null load-order group,
    // tag, dependencies, account (LocalSystem) and password.
    sc_result(unsafe {
        CreateServiceW(
            manager.0,
            name.as_ptr(),
            display.as_ptr(),
            access,
            SERVICE_WIN32_OWN_PROCESS,
            SERVICE_AUTO_START,
            SERVICE_ERROR_NORMAL,
            command.as_ptr(),
            ptr::null(),
            ptr::null_mut(),
            ptr::null(),
            ptr::null(),
            ptr::null(),
        )
    })
}

/// `OpenServiceW`.
pub(crate) fn open_service(manager: &ScHandle, name: &OsStr, access: u32) -> io::Result<ScHandle> {
    let name = wide(name);
    // SAFETY: `name` is NUL-terminated and outlives the call.
    sc_result(unsafe { OpenServiceW(manager.0, name.as_ptr(), access) })
}

/// Sets a service's description, and has the SCM restart it `restarts` times, `delay_ms`
/// apart, after failures.
pub(crate) fn configure_service(
    service: &ScHandle,
    description: &OsStr,
    restarts: usize,
    delay_ms: u32,
) -> io::Result<()> {
    let mut text = wide(description);
    let info = SERVICE_DESCRIPTIONW {
        lpDescription: text.as_mut_ptr(),
    };
    // SAFETY: `info` points to a NUL-terminated string; both outlive the call.
    if unsafe {
        ChangeServiceConfig2W(
            service.0,
            SERVICE_CONFIG_DESCRIPTION,
            (&raw const info).cast(),
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    let mut actions: Vec<SC_ACTION> = (0..restarts)
        .map(|_| SC_ACTION {
            Type: SC_ACTION_RESTART,
            Delay: delay_ms,
        })
        .collect();
    let failure = SERVICE_FAILURE_ACTIONSW {
        dwResetPeriod: 24 * 60 * 60,
        lpRebootMsg: ptr::null_mut(),
        lpCommand: ptr::null_mut(),
        cActions: len_u32(actions.len()),
        lpsaActions: actions.as_mut_ptr(),
    };
    // SAFETY: `failure` and the actions it points to outlive the call.
    if unsafe {
        ChangeServiceConfig2W(
            service.0,
            SERVICE_CONFIG_FAILURE_ACTIONS,
            (&raw const failure).cast(),
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// `StartServiceW` with no arguments.
pub(crate) fn start_service(service: &ScHandle) -> io::Result<()> {
    // SAFETY: no arguments: count 0 and a null vector.
    if unsafe { StartServiceW(service.0, 0, ptr::null()) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// `ControlService`; returns the service's state after the control.
pub(crate) fn control_service(service: &ScHandle, control: u32) -> io::Result<u32> {
    let mut status = MaybeUninit::<SERVICE_STATUS>::zeroed();
    // SAFETY: `status` is a valid out buffer.
    if unsafe { ControlService(service.0, control, status.as_mut_ptr()) } == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the call succeeded and filled `status`.
    Ok(unsafe { status.assume_init() }.dwCurrentState)
}

/// `QueryServiceStatus`: the service's current state.
pub(crate) fn service_state(service: &ScHandle) -> io::Result<u32> {
    let mut status = MaybeUninit::<SERVICE_STATUS>::zeroed();
    // SAFETY: `status` is a valid out buffer.
    if unsafe { QueryServiceStatus(service.0, status.as_mut_ptr()) } == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the call succeeded and filled `status`.
    Ok(unsafe { status.assume_init() }.dwCurrentState)
}

/// `DeleteService`: marks the service for deletion once its handles close.
pub(crate) fn delete_service(service: &ScHandle) -> io::Result<()> {
    // SAFETY: plain call on an open service handle.
    if unsafe { DeleteService(service.0) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// What runs as the service: set once by [`service_dispatch`].
static SERVICE_BODY: OnceLock<fn(ServiceStatus)> = OnceLock::new();
/// Called on stop, shutdown and preshutdown.
static SERVICE_STOP: OnceLock<fn()> = OnceLock::new();

/// The handle a service reports its state through.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ServiceStatus(SERVICE_STATUS_HANDLE);

// SAFETY: a service status handle is a token the SCM hands out for `SetServiceStatus`,
// which may be called from any thread.
unsafe impl Send for ServiceStatus {}
// SAFETY: as above.
unsafe impl Sync for ServiceStatus {}

impl ServiceStatus {
    /// `SetServiceStatus`. A running service accepts stop, shutdown and preshutdown.
    pub(crate) fn set(self, state: u32, exit_code: u32, wait_hint_ms: u32) {
        let status = SERVICE_STATUS {
            dwServiceType: SERVICE_WIN32_OWN_PROCESS,
            dwCurrentState: state,
            dwControlsAccepted: if state == SERVICE_RUNNING {
                SERVICE_ACCEPT_STOP | SERVICE_ACCEPT_SHUTDOWN | SERVICE_ACCEPT_PRESHUTDOWN
            } else {
                0
            },
            dwWin32ExitCode: exit_code,
            dwServiceSpecificExitCode: 0,
            dwCheckPoint: 0,
            dwWaitHint: wait_hint_ms,
        };
        // SAFETY: the handle came from `RegisterServiceCtrlHandlerExW`; `status` is valid.
        unsafe { SetServiceStatus(self.0, &raw const status) };
    }
}

unsafe extern "system" fn service_handler(
    control: u32,
    _event: u32,
    _data: *mut c_void,
    _context: *mut c_void,
) -> u32 {
    match control {
        SERVICE_CONTROL_INTERROGATE => NO_ERROR,
        SERVICE_CONTROL_STOP | SERVICE_CONTROL_SHUTDOWN | SERVICE_CONTROL_PRESHUTDOWN => {
            if let Some(stop) = SERVICE_STOP.get() {
                stop();
            }
            NO_ERROR
        }
        _ => ERROR_CALL_NOT_IMPLEMENTED,
    }
}

unsafe extern "system" fn service_main(_argc: u32, _argv: *mut PWSTR) {
    let name = [0u16];
    // SAFETY: an own-process service may register with an empty name; the handler is a
    // valid function for the life of the process and needs no context.
    let handle =
        unsafe { RegisterServiceCtrlHandlerExW(name.as_ptr(), Some(service_handler), ptr::null()) };
    if handle.is_null() {
        return;
    }
    if let Some(body) = SERVICE_BODY.get() {
        body(ServiceStatus(handle));
    }
}

/// `StartServiceCtrlDispatcherW`: runs `body` as this process's service, calling `stop`
/// when the SCM asks it to stop. Returns once the service has stopped. Fails at once (error
/// 1063) when the process wasn't started by the SCM.
pub(crate) fn service_dispatch(body: fn(ServiceStatus), stop: fn()) -> io::Result<()> {
    let _ = SERVICE_BODY.set(body);
    let _ = SERVICE_STOP.set(stop);
    let mut name = [0u16];
    let table = [
        SERVICE_TABLE_ENTRYW {
            lpServiceName: name.as_mut_ptr(),
            lpServiceProc: Some(service_main),
        },
        SERVICE_TABLE_ENTRYW {
            lpServiceName: ptr::null_mut(),
            lpServiceProc: None,
        },
    ];
    // SAFETY: `table` ends with a null entry and outlives the call, which returns only when
    // the service has stopped.
    if unsafe { StartServiceCtrlDispatcherW(table.as_ptr()) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[allow(clippy::cast_possible_wrap)] // Win32 error codes are small positive numbers
fn code(err: u32) -> i32 {
    err as i32
}

fn len_u32(len: usize) -> u32 {
    u32::try_from(len).unwrap_or(u32::MAX)
}
