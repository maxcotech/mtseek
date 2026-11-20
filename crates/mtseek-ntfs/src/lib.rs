//! NTFS master file table enumeration and USN change journal reading.
//!
//! Two layers:
//!
//! * [`record`]: a safe, portable parser for the USN record buffers that NTFS returns.
//!   It is pure byte parsing, so it is property-tested on any machine, no admin needed.
//! * [`Volume`] (Windows only): opens `\\.\C:` and issues the three control codes mtseek
//!   needs. All `unsafe` in the project lives in this crate's private `sys` module.
//!
//! Opening a volume needs administrator rights. Without them [`Volume::open`] returns
//! [`VolumeError::NotElevated`], and callers fall back to walking directories.

pub mod record;
#[cfg(windows)]
mod sys;
#[cfg(windows)]
mod volume;

pub use record::{RecordError, Records, UsnRecord};
#[cfg(windows)]
pub use volume::{JournalInfo, Volume, VolumeError, file_ref};

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
