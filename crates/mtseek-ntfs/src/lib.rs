//! NTFS access for the name index: master file table enumeration, change journal reads, and
//! directory listings that carry file reference numbers.
//!
//! * [`record`], [`dir`] and [`mft`]: safe, portable parsers for the buffers NTFS returns. They are
//!   pure byte parsing, so they are property-tested on any machine, no admin needed.
//! * [`Volume`] (Windows only, needs admin): enumerates the MFT through `\\.\C:`. Without
//!   admin rights [`Volume::open`] returns [`VolumeError::NotElevated`], and callers walk
//!   directories instead.
//! * [`Journal`] (Windows only, no admin needed): the volume's USN change journal, read
//!   through a directory handle. Unprivileged records carry no names, only the file's and
//!   its parent directory's reference numbers.
//! * [`Directory`] (Windows only): lists a directory with each entry's reference number.
//! * [`pipe`] (Windows only): the daemon's local named pipe, with its access checks
//!   (ADR 0013). It lives here because it needs `unsafe` Win32 calls too.
//!
//! All `unsafe` in the project lives in this crate's private `sys` module.

pub mod dir;
#[cfg(windows)]
mod journal;
pub mod mft;
#[cfg(windows)]
pub mod pipe;
pub mod record;
#[cfg(windows)]
mod sys;
#[cfg(windows)]
mod volume;

pub use dir::DirEntry;
#[cfg(windows)]
pub use dir::{DirBuffer, Directory};
#[cfg(windows)]
pub use journal::{Journal, JournalInfo};
pub use record::{RecordError, Records, UsnRecord};
#[cfg(windows)]
pub use volume::{MftStats, NtfsGeometry, Volume, VolumeError, file_ref, file_ref_of};

/// Splits an NTFS file reference number into (MFT record number, sequence number).
///
/// The low 48 bits index the MFT and the high 16 bits count how often that MFT slot has
/// been reused. Two references are the same file only if both parts match.
pub fn split_file_ref(file_ref: u64) -> (u64, u16) {
    #[allow(clippy::cast_possible_truncation)] // the shift leaves exactly 16 bits
    (file_ref & 0x0000_FFFF_FFFF_FFFF, (file_ref >> 48) as u16)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_record_number_and_sequence() {
        assert_eq!(split_file_ref(0x0005_0000_0000_0005), (5, 5));
        assert_eq!(split_file_ref(0x0001_0000_0012_3456), (0x12_3456, 1));
    }
}
