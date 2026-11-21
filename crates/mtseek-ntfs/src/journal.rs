//! Reading a volume's USN change journal without administrator rights.
//!
//! Since Windows 10 1709, `FSCTL_QUERY_USN_JOURNAL` and
//! `FSCTL_READ_UNPRIVILEGED_USN_JOURNAL` work for any user on a handle to a *directory* of
//! the volume (a volume handle still needs admin). The catch: unprivileged records carry no
//! file names. They still carry the file's and its parent directory's reference numbers
//! and the reason for the change, which is enough to know which directories to re-list.

use std::io;
use std::os::windows::io::{AsHandle, OwnedHandle};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use windows_sys::Win32::Foundation::{ERROR_JOURNAL_ENTRY_DELETED, ERROR_JOURNAL_NOT_ACTIVE};
use windows_sys::Win32::Storage::FileSystem::{FILE_FLAG_BACKUP_SEMANTICS, FILE_READ_ATTRIBUTES};
use windows_sys::Win32::System::Ioctl::{
    FSCTL_QUERY_USN_JOURNAL, FSCTL_READ_UNPRIVILEGED_USN_JOURNAL,
};

use crate::VolumeError;
use crate::record::{Records, UsnRecord};
use crate::sys;

/// Output buffer size for journal reads. 1 MiB holds ~10k records per call.
const BUFFER_SIZE: usize = 1 << 20;

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

/// A volume's change journal, opened through its root directory. No admin rights needed.
#[derive(Debug)]
pub struct Journal {
    handle: OwnedHandle,
    letter: char,
}

impl Journal {
    /// Opens the journal of the volume mounted at drive `letter`.
    pub fn open(letter: char) -> Result<Self, VolumeError> {
        if !letter.is_ascii_alphabetic() {
            return Err(VolumeError::BadLetter(letter));
        }
        let letter = letter.to_ascii_uppercase();
        let root = format!(r"{letter}:\");
        // FILE_FLAG_BACKUP_SEMANTICS is required to open a directory handle.
        let handle = sys::open(
            root.as_ref(),
            FILE_READ_ATTRIBUTES,
            FILE_FLAG_BACKUP_SEMANTICS,
        )
        .map_err(|source| VolumeError::Io { letter, source })?;
        Ok(Self { handle, letter })
    }

    /// The journal's identity and current range (`FSCTL_QUERY_USN_JOURNAL`).
    pub fn info(&self) -> Result<JournalInfo, VolumeError> {
        let mut out = [0u8; 64]; // USN_JOURNAL_DATA_V0 is 56 bytes
        sys::ioctl(
            self.handle.as_handle(),
            FSCTL_QUERY_USN_JOURNAL,
            &[],
            &mut out,
        )
        .map_err(|e| self.error(e))?;
        Ok(JournalInfo {
            journal_id: u64::from_le_bytes(word(&out, 0)),
            first_usn: i64::from_le_bytes(word(&out, 8)),
            next_usn: i64::from_le_bytes(word(&out, 16)),
        })
    }

    /// Delivers every record from `from_usn` up to the current end of the journal.
    /// Records have empty names unless the caller is elevated.
    ///
    /// Returns the position to resume from next time. Fails with
    /// [`VolumeError::JournalWrapped`] if `from_usn` is older than the journal keeps.
    pub fn read(
        &self,
        journal: &JournalInfo,
        from_usn: i64,
        on_record: &mut dyn FnMut(UsnRecord),
    ) -> Result<i64, VolumeError> {
        if from_usn < journal.first_usn {
            return Err(VolumeError::JournalWrapped(self.letter));
        }
        let mut out = vec![0u8; BUFFER_SIZE];
        let mut usn = from_usn;
        loop {
            // READ_USN_JOURNAL_DATA_V0 { StartUsn, ReasonMask, ReturnOnlyOnClose, Timeout,
            //                            BytesToWaitFor, UsnJournalID }
            let mut input = [0u8; 40];
            input[0..8].copy_from_slice(&usn.to_le_bytes());
            input[8..12].copy_from_slice(&u32::MAX.to_le_bytes());
            input[32..40].copy_from_slice(&journal.journal_id.to_le_bytes());
            let returned = match sys::ioctl(
                self.handle.as_handle(),
                FSCTL_READ_UNPRIVILEGED_USN_JOURNAL,
                &input,
                &mut out,
            ) {
                Ok(n) => n,
                Err(e) if e.raw_os_error() == code(ERROR_JOURNAL_ENTRY_DELETED) => {
                    return Err(VolumeError::JournalWrapped(self.letter));
                }
                Err(e) => return Err(self.error(e)),
            };
            if returned < 8 {
                return Ok(usn);
            }
            let next = i64::from_le_bytes(word(&out, 0));
            for record in Records::new(&out[8..returned]) {
                on_record(record.map_err(|source| VolumeError::BadRecord {
                    letter: self.letter,
                    source,
                })?);
            }
            if returned == 8 || next <= usn {
                return Ok(next.max(usn));
            }
            usn = next;
        }
    }

    /// The journal position of the first change at most `age` old: where to replay from
    /// to cover everything that happened in the last `age`. Reads the whole journal, which
    /// takes tens of milliseconds, so it is for full builds, not every run.
    pub fn position_since(&self, journal: &JournalInfo, age: Duration) -> Result<i64, VolumeError> {
        let cutoff =
            filetime_now().saturating_sub(i64::try_from(age.as_nanos() / 100).unwrap_or(i64::MAX));
        let mut found = None;
        let next = self.read(journal, journal.first_usn, &mut |r| {
            if found.is_none() && r.timestamp >= cutoff {
                found = Some(r.usn);
            }
        })?;
        Ok(found.unwrap_or(next))
    }

    fn error(&self, source: io::Error) -> VolumeError {
        if source.raw_os_error() == code(ERROR_JOURNAL_NOT_ACTIVE) {
            return VolumeError::NoJournal(self.letter);
        }
        VolumeError::Io {
            letter: self.letter,
            source,
        }
    }
}

/// Now, as a Windows `FILETIME`: 100 ns ticks since 1601-01-01 UTC.
fn filetime_now() -> i64 {
    const UNIX_EPOCH_AS_FILETIME: i64 = 116_444_736_000_000_000;
    let since_unix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_nanos() / 100).unwrap_or(i64::MAX));
    UNIX_EPOCH_AS_FILETIME.saturating_add(since_unix)
}

fn word(buf: &[u8], at: usize) -> [u8; 8] {
    let mut w = [0; 8];
    w.copy_from_slice(&buf[at..at + 8]);
    w
}

fn code(err: u32) -> Option<i32> {
    i32::try_from(err).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::record::reason;

    /// Runs as any user: the unprivileged journal must report a new file's directory.
    #[test]
    fn reports_changes_without_admin() {
        let dir = tempfile::tempdir().unwrap();
        let letter = dir.path().to_string_lossy().chars().next().unwrap();
        let journal = Journal::open(letter).unwrap();
        let info = journal.info().unwrap();
        assert!(info.next_usn >= info.first_usn);

        let dir_ref = crate::file_ref(dir.path()).unwrap();
        std::fs::write(dir.path().join("mtseek-journal-probe.txt"), "x").unwrap();
        let file_ref = crate::file_ref(&dir.path().join("mtseek-journal-probe.txt")).unwrap();

        let mut created = false;
        let next = journal
            .read(&info, info.next_usn, &mut |r| {
                created |= r.parent_ref == dir_ref
                    && r.file_ref == file_ref
                    && r.reason & reason::FILE_CREATE != 0;
            })
            .unwrap();
        assert!(created, "journal did not report the new file");
        assert!(next > info.next_usn);
    }

    #[test]
    fn finds_the_position_of_recent_changes() {
        let dir = tempfile::tempdir().unwrap();
        let letter = dir.path().to_string_lossy().chars().next().unwrap();
        let journal = Journal::open(letter).unwrap();
        let path = dir.path().join("mtseek-recent.txt");
        std::fs::write(&path, "x").unwrap();
        let created = crate::file_ref(&path).unwrap();
        let info = journal.info().unwrap();

        // Replaying the last minute must include the file just created, with a real time.
        let since = journal
            .position_since(&info, Duration::from_secs(60))
            .unwrap();
        assert!(since >= info.first_usn && since < info.next_usn);
        let mut create_time = None;
        journal
            .read(&info, since, &mut |r| {
                if r.file_ref == created && r.reason & crate::record::reason::FILE_CREATE != 0 {
                    create_time = Some(r.timestamp);
                }
            })
            .unwrap();
        let create_time = create_time.expect("the create is within the last minute");
        assert!(
            (filetime_now() - create_time).abs() < 600_000_000,
            "within a minute"
        );

        // Nothing is newer than now, so a zero age points past the newest record.
        let none = journal.position_since(&info, Duration::ZERO).unwrap();
        assert!(none > since);
    }

    #[test]
    fn a_position_before_the_journal_start_is_wrapped() {
        let journal = Journal::open('C').unwrap();
        let info = journal.info().unwrap();
        let result = journal.read(&info, info.first_usn - 1, &mut |_| {});
        assert!(matches!(result, Err(VolumeError::JournalWrapped('C'))));
    }
}
