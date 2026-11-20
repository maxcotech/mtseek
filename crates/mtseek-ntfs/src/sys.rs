//! Raw Win32 calls: the only module in mtseek that contains `unsafe`.
//!
//! Each function wraps one API, checks its result, and hands back safe Rust types
//! (`OwnedHandle`, byte counts, `io::Error`). Keep it that small.

#![allow(unsafe_code)]

use std::ffi::OsStr;
use std::io;
use std::mem::MaybeUninit;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, BorrowedHandle, FromRawHandle, OwnedHandle};
use std::ptr;

use windows_sys::Win32::Foundation::{ERROR_NO_MORE_FILES, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, CreateFileW, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    FileIdBothDirectoryInfo, FileIdBothDirectoryRestartInfo, GetFileInformationByHandle,
    GetFileInformationByHandleEx, OPEN_EXISTING,
};
use windows_sys::Win32::System::IO::DeviceIoControl;

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
pub(crate) fn file_index(handle: &OwnedHandle) -> io::Result<u64> {
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

fn len_u32(len: usize) -> u32 {
    u32::try_from(len).unwrap_or(u32::MAX)
}
