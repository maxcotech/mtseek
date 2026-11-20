//! A handle to an NTFS volume and the three control codes mtseek uses on it.

use std::io;
use std::os::windows::io::OwnedHandle;
use std::path::Path;

use windows_sys::Win32::Foundation::{
    ERROR_ACCESS_DENIED, ERROR_HANDLE_EOF, ERROR_JOURNAL_ENTRY_DELETED, ERROR_JOURNAL_NOT_ACTIVE,
    GENERIC_READ,
};
use windows_sys::Win32::Storage::FileSystem::{FILE_FLAG_BACKUP_SEMANTICS, FILE_READ_ATTRIBUTES};
use windows_sys::Win32::System::Ioctl::{
    FSCTL_ENUM_USN_DATA, FSCTL_QUERY_USN_JOURNAL, FSCTL_READ_USN_JOURNAL,
};

use crate::record::{Records, UsnRecord};
use crate::sys;

/// Output buffer size for enumeration and journal reads. 1 MiB holds ~10k records per call.
const BUFFER_SIZE: usize = 1 << 20;

/// Why a volume operation failed.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum VolumeError {
    /// Opening a volume for reading needs administrator rights.
    #[error("reading the NTFS master file table needs administrator rights")]
    NotElevated,
    /// The argument was not a drive letter.
    #[error("`{0}` is not a drive letter")]
    BadLetter(char),
    /// The volume has no USN change journal (it may not be NTFS, or the journal was deleted).
    #[error("drive {0}: has no active USN change journal")]
    NoJournal(char),
    /// The journal no longer holds the requested position: changes were lost and the
    /// caller must rebuild from a fresh enumeration.
    #[error("drive {0}: the change journal has wrapped past the saved position")]
    JournalWrapped(char),
    /// The kernel returned a buffer mtseek could not parse.
    #[error("drive {letter}: malformed USN record: {source}")]
    BadRecord {
        /// The drive letter.
        letter: char,
        /// What was wrong with it.
        #[source]
        source: crate::RecordError,
    },
    /// Any other I/O error.
    #[error("drive {letter}: {source}")]
    Io {
        /// The drive letter.
        letter: char,
        /// The underlying error.
        #[source]
        source: io::Error,
    },
}

/// Identity and range of a volume's change journal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JournalInfo {
    /// Changes when the journal is deleted and recreated; saved positions are then invalid.
    pub journal_id: u64,
    /// Oldest position still in the journal.
    pub first_usn: i64,
    /// Position the next change will get.
    pub next_usn: i64,
}

/// An open NTFS volume, e.g. `\\.\C:`.
#[derive(Debug)]
pub struct Volume {
    handle: OwnedHandle,
    letter: char,
}

impl Volume {
    /// Opens a volume by drive letter. Needs administrator rights.
    pub fn open(letter: char) -> Result<Self, VolumeError> {
        if !letter.is_ascii_alphabetic() {
            return Err(VolumeError::BadLetter(letter));
        }
        let letter = letter.to_ascii_uppercase();
        let path = format!(r"\\.\{letter}:");
        match sys::open(path.as_ref(), GENERIC_READ, 0) {
            Ok(handle) => Ok(Self { handle, letter }),
            Err(e) if e.raw_os_error() == Some(code(ERROR_ACCESS_DENIED)) => {
                Err(VolumeError::NotElevated)
            }
            Err(source) => Err(VolumeError::Io { letter, source }),
        }
    }

    /// The drive letter, uppercase.
    pub fn letter(&self) -> char {
        self.letter
    }

    /// Queries the change journal (`FSCTL_QUERY_USN_JOURNAL`).
    pub fn journal(&self) -> Result<JournalInfo, VolumeError> {
        let mut out = [0u8; 64]; // USN_JOURNAL_DATA_V0 is 56 bytes
        sys::ioctl(&self.handle, FSCTL_QUERY_USN_JOURNAL, &[], &mut out)
            .map_err(|e| self.error(e))?;
        Ok(JournalInfo {
            journal_id: u64::from_le_bytes(word(&out, 0)),
            first_usn: i64::from_le_bytes(word(&out, 8)),
            next_usn: i64::from_le_bytes(word(&out, 16)),
        })
    }

    /// Lists every file and directory on the volume (`FSCTL_ENUM_USN_DATA`).
    ///
    /// Records arrive in MFT order, so a child can come before its parent directory.
    /// Returns the number of records delivered.
    pub fn enumerate(&self, on_record: &mut dyn FnMut(UsnRecord)) -> Result<u64, VolumeError> {
        let mut out = vec![0u8; BUFFER_SIZE];
        let mut start_ref = 0u64;
        let mut count = 0u64;
        loop {
            // MFT_ENUM_DATA_V0 { StartFileReferenceNumber, LowUsn, HighUsn }
            let mut input = [0u8; 24];
            input[0..8].copy_from_slice(&start_ref.to_le_bytes());
            input[16..24].copy_from_slice(&i64::MAX.to_le_bytes());
            let returned = match sys::ioctl(&self.handle, FSCTL_ENUM_USN_DATA, &input, &mut out) {
                Ok(n) => n,
                Err(e) if e.raw_os_error() == Some(code(ERROR_HANDLE_EOF)) => return Ok(count),
                Err(e) => return Err(self.error(e)),
            };
            if returned <= 8 {
                return Ok(count);
            }
            start_ref = u64::from_le_bytes(word(&out, 0));
            for record in Records::new(&out[8..returned]) {
                on_record(record.map_err(|source| self.bad_record(source))?);
                count += 1;
            }
        }
    }

    /// Reads journal records from `from_usn` up to the current end (`FSCTL_READ_USN_JOURNAL`).
    ///
    /// Returns the position to resume from next time.
    pub fn read_changes(
        &self,
        journal: &JournalInfo,
        from_usn: i64,
        on_record: &mut dyn FnMut(UsnRecord),
    ) -> Result<i64, VolumeError> {
        let mut out = vec![0u8; BUFFER_SIZE];
        let mut usn = from_usn;
        loop {
            // READ_USN_JOURNAL_DATA_V0 { StartUsn, ReasonMask, ReturnOnlyOnClose, Timeout,
            //                            BytesToWaitFor, UsnJournalID }
            let mut input = [0u8; 40];
            input[0..8].copy_from_slice(&usn.to_le_bytes());
            input[8..12].copy_from_slice(&u32::MAX.to_le_bytes());
            input[32..40].copy_from_slice(&journal.journal_id.to_le_bytes());
            let returned = match sys::ioctl(&self.handle, FSCTL_READ_USN_JOURNAL, &input, &mut out)
            {
                Ok(n) => n,
                Err(e) if e.raw_os_error() == Some(code(ERROR_JOURNAL_ENTRY_DELETED)) => {
                    return Err(VolumeError::JournalWrapped(self.letter));
                }
                Err(e) => return Err(self.error(e)),
            };
            let next = i64::from_le_bytes(word(&out, 0));
            if returned <= 8 || next == usn {
                return Ok(next);
            }
            for record in Records::new(&out[8..returned]) {
                on_record(record.map_err(|source| self.bad_record(source))?);
            }
            usn = next;
        }
    }

    fn error(&self, source: io::Error) -> VolumeError {
        if source.raw_os_error() == Some(code(ERROR_JOURNAL_NOT_ACTIVE)) {
            return VolumeError::NoJournal(self.letter);
        }
        VolumeError::Io {
            letter: self.letter,
            source,
        }
    }

    fn bad_record(&self, source: crate::RecordError) -> VolumeError {
        VolumeError::BadRecord {
            letter: self.letter,
            source,
        }
    }
}

/// The NTFS file reference number of a file or directory. Does not need administrator rights.
pub fn file_ref(path: &Path) -> io::Result<u64> {
    // FILE_FLAG_BACKUP_SEMANTICS is required to open a directory handle.
    let handle = sys::open(
        path.as_os_str(),
        FILE_READ_ATTRIBUTES,
        FILE_FLAG_BACKUP_SEMANTICS,
    )?;
    sys::file_index(&handle)
}

fn word(buf: &[u8], at: usize) -> [u8; 8] {
    let mut w = [0; 8];
    w.copy_from_slice(&buf[at..at + 8]);
    w
}

#[allow(clippy::cast_possible_wrap)] // Win32 error codes are small positive numbers
fn code(err: u32) -> i32 {
    err as i32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_refs_are_stable_and_distinct() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "a").unwrap();
        std::fs::write(dir.path().join("b.txt"), "b").unwrap();
        let a = file_ref(&dir.path().join("a.txt")).unwrap();
        let b = file_ref(&dir.path().join("b.txt")).unwrap();
        assert_ne!(a, b);
        assert_eq!(a, file_ref(&dir.path().join("a.txt")).unwrap());
        assert!(
            file_ref(dir.path()).is_ok(),
            "directories need FILE_FLAG_BACKUP_SEMANTICS"
        );
    }

    #[test]
    fn rejects_non_letters() {
        assert!(matches!(
            Volume::open('1'),
            Err(VolumeError::BadLetter('1'))
        ));
    }

    /// Runs only with administrator rights. CI runners are elevated and set
    /// `MTSEEK_REQUIRE_ADMIN=1`, which turns a skip into a failure.
    #[test]
    fn enumerates_the_system_volume_when_elevated() {
        let volume = match Volume::open('C') {
            Ok(v) => v,
            Err(VolumeError::NotElevated) => {
                assert!(
                    std::env::var_os("MTSEEK_REQUIRE_ADMIN").is_none(),
                    "MTSEEK_REQUIRE_ADMIN is set but the test is not elevated"
                );
                return;
            }
            Err(e) => panic!("{e}"),
        };
        let journal = volume.journal().unwrap();
        assert!(journal.next_usn >= journal.first_usn);

        let windows = file_ref(Path::new(r"C:\Windows")).unwrap();
        let mut found_windows = false;
        let mut count = 0u64;
        volume
            .enumerate(&mut |r| {
                count += 1;
                found_windows |= r.file_ref == windows && r.name.eq_ignore_ascii_case("Windows");
            })
            .unwrap();
        assert!(count > 10_000, "only {count} records on C:");
        assert!(
            found_windows,
            "C:\\Windows missing from the MFT enumeration"
        );

        // A file created now must show up in the journal.
        let dir = tempfile::tempdir().unwrap();
        let start = volume.journal().unwrap().next_usn;
        std::fs::write(dir.path().join("mtseek-journal-probe.txt"), "x").unwrap();
        let mut seen = false;
        volume
            .read_changes(&journal, start, &mut |r| {
                seen |= r.name == "mtseek-journal-probe.txt";
            })
            .unwrap();
        assert!(seen, "journal did not report the new file");
    }
}
