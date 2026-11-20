//! A handle to an NTFS volume, for reading its master file table (needs admin).

use std::fs::File;
use std::io;
use std::os::windows::fs::FileExt;
use std::os::windows::io::AsHandle;
use std::path::Path;
use std::sync::mpsc::sync_channel;

use windows_sys::Win32::Foundation::{
    ERROR_ACCESS_DENIED, ERROR_HANDLE_EOF, ERROR_JOURNAL_NOT_ACTIVE, GENERIC_READ,
};
use windows_sys::Win32::Storage::FileSystem::{FILE_FLAG_BACKUP_SEMANTICS, FILE_READ_ATTRIBUTES};
use windows_sys::Win32::System::Ioctl::{FSCTL_ENUM_USN_DATA, FSCTL_GET_NTFS_VOLUME_DATA};

use crate::mft::{self, Extent, FileName, MftError};
use crate::record::{Records, UsnRecord};
use crate::sys;

/// Raw MFT reads go in chunks this big: large enough to keep the disk busy, small enough
/// that a few in flight cost little memory. A multiple of every sector, cluster and record
/// size NTFS supports.
const MFT_CHUNK: usize = 4 << 20;

/// Output buffer size for enumeration. 1 MiB holds ~10k records per call.
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
    /// The master file table could not be parsed. Callers fall back to
    /// [`Volume::enumerate`].
    #[error("drive {letter}: cannot read the master file table: {source}")]
    BadMft {
        /// The drive letter.
        letter: char,
        /// What was wrong.
        #[source]
        source: MftError,
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

/// An open NTFS volume, e.g. `\\.\C:`.
#[derive(Debug)]
pub struct Volume {
    file: File,
    letter: char,
}

/// Where the master file table is, and the sizes needed to read it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NtfsGeometry {
    /// Logical sector size: raw reads must be aligned to it.
    pub bytes_per_sector: u32,
    /// Cluster size.
    pub bytes_per_cluster: u32,
    /// Size of one MFT record (1 KiB, or 4 KiB on 4K-native disks).
    pub record_size: u32,
    /// Bytes of the MFT that hold records; beyond it is preallocated space.
    pub mft_valid_length: u64,
    /// The cluster where the MFT starts (where record 0, `$MFT` itself, is).
    pub mft_start_lcn: u64,
}

/// What [`Volume::read_mft`] read.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MftStats {
    /// Records examined, used or not.
    pub records: u64,
    /// Names delivered.
    pub names: u64,
    /// Records skipped because they failed to parse.
    pub bad_records: u64,
    /// Bytes read from the volume.
    pub bytes_read: u64,
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
            Ok(handle) => Ok(Self {
                file: File::from(handle),
                letter,
            }),
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
            let returned =
                match sys::ioctl(self.file.as_handle(), FSCTL_ENUM_USN_DATA, &input, &mut out) {
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

    /// The MFT's location and the volume's sizes (`FSCTL_GET_NTFS_VOLUME_DATA`).
    pub fn geometry(&self) -> Result<NtfsGeometry, VolumeError> {
        let mut out = [0u8; 96]; // NTFS_VOLUME_DATA_BUFFER
        sys::ioctl(
            self.file.as_handle(),
            FSCTL_GET_NTFS_VOLUME_DATA,
            &[],
            &mut out,
        )
        .map_err(|e| self.error(e))?;
        let u32_at =
            |at: usize| u32::from_le_bytes([out[at], out[at + 1], out[at + 2], out[at + 3]]);
        let u64_at = |at: usize| u64::from_le_bytes(word(&out, at));
        Ok(NtfsGeometry {
            bytes_per_sector: u32_at(40),
            bytes_per_cluster: u32_at(44),
            record_size: u32_at(48),
            mft_valid_length: u64_at(56),
            mft_start_lcn: u64_at(64),
        })
    }

    /// Reads every name of every file from the raw master file table, hard links included.
    ///
    /// Unlike [`Self::enumerate`], a file with several hard links yields one [`FileName`]
    /// per link. Records arrive in MFT order, so a child can come before its directory.
    /// Records that fail to parse are counted in [`MftStats::bad_records`] and skipped.
    ///
    /// The on-disk MFT can lag a few seconds behind what NTFS holds in memory, so callers
    /// should replay the change journal from a little before the read started.
    pub fn read_mft(&self, on_name: &mut dyn FnMut(FileName)) -> Result<MftStats, VolumeError> {
        let g = self.geometry()?;
        let record_size = g.record_size as usize;
        let record_bytes = u64::from(g.record_size);
        let cluster = u64::from(g.bytes_per_cluster);
        let sizes_ok = [g.bytes_per_sector, g.bytes_per_cluster, g.record_size]
            .iter()
            .all(|&n| n >= 512 && n.is_power_of_two() && n as usize <= MFT_CHUNK);
        if !sizes_ok || g.mft_valid_length < record_bytes {
            return Err(self.bad_mft(MftError::Unsupported("unusual volume geometry")));
        }
        let valid_records = g.mft_valid_length / record_bytes;
        // The smallest read that is a whole number of sectors, clusters and records.
        let unit = u64::from(
            g.bytes_per_sector
                .max(g.bytes_per_cluster)
                .max(g.record_size),
        );

        // Record 0 is $MFT itself. Its $DATA run list says where every other record is.
        let first = Extent {
            vcn: 0,
            lcn: g.mft_start_lcn,
            clusters: unit.div_ceil(cluster),
        };
        let mut record0 = self.read_record(&[first], &g, 0)?;
        mft::apply_fixups(&mut record0).map_err(|e| self.bad_mft(e))?;
        let mut extents = mft::data_extents(&record0).map_err(|e| self.bad_mft(e))?;
        let continued =
            mft::data_records_in_attribute_list(&record0, 0).map_err(|e| self.bad_mft(e))?;
        for number in continued {
            let mut record = self.read_record(&extents, &g, number)?;
            mft::apply_fixups(&mut record).map_err(|e| self.bad_mft(e))?;
            extents.extend(mft::data_extents(&record).map_err(|e| self.bad_mft(e))?);
        }
        extents.sort_unstable();
        extents.dedup();
        let mut expected_vcn = 0;
        for e in &extents {
            let aligned = (e.vcn * cluster).is_multiple_of(record_bytes)
                && (e.clusters * cluster).is_multiple_of(record_bytes);
            if e.vcn != expected_vcn || !aligned {
                return Err(self.bad_mft(MftError::Unsupported("MFT run list has gaps")));
            }
            expected_vcn = e.vcn + e.clusters;
        }

        // One thread reads chunks while this one parses them.
        std::thread::scope(|scope| {
            let (send, receive) = sync_channel::<Result<(u64, Vec<u8>), VolumeError>>(4);
            let extents = &extents;
            scope.spawn(move || {
                for e in extents {
                    let stream_start = e.vcn * cluster;
                    if stream_start >= g.mft_valid_length {
                        break;
                    }
                    // Up to the valid length, rounded up to a whole read unit.
                    let extent_bytes = e.clusters * cluster;
                    let wanted = (g.mft_valid_length - stream_start)
                        .min(extent_bytes)
                        .next_multiple_of(unit)
                        .min(extent_bytes);
                    let mut done = 0;
                    while done < wanted {
                        let len = (wanted - done).min(MFT_CHUNK as u64);
                        #[allow(clippy::cast_possible_truncation)] // at most MFT_CHUNK
                        let mut buf = vec![0u8; len as usize];
                        let read = self.read_at(e.lcn * cluster + done, &mut buf);
                        let first_record = (stream_start + done) / record_bytes;
                        let failed = read.is_err();
                        if send.send(read.map(|()| (first_record, buf))).is_err() || failed {
                            return; // the parser stopped, or there is nothing more to read
                        }
                        done += len;
                    }
                }
            });
            let mut stats = MftStats::default();
            let mut names = Vec::new();
            for chunk in receive {
                let (first_record, mut buf) = chunk?;
                stats.bytes_read += buf.len() as u64;
                for (i, record) in buf.chunks_exact_mut(record_size).enumerate() {
                    let number = first_record + i as u64;
                    if number >= valid_records {
                        break;
                    }
                    stats.records += 1;
                    if mft::parse_record(record, number, &mut names).is_err() {
                        stats.bad_records += 1;
                        names.clear();
                    }
                    stats.names += names.len() as u64;
                    for name in names.drain(..) {
                        on_name(name);
                    }
                }
            }
            Ok(stats)
        })
    }

    /// Reads MFT record `number` (fixups not applied), using the extents known so far.
    fn read_record(
        &self,
        extents: &[Extent],
        g: &NtfsGeometry,
        number: u64,
    ) -> Result<Vec<u8>, VolumeError> {
        let cluster = u64::from(g.bytes_per_cluster);
        let unit = u64::from(
            g.bytes_per_sector
                .max(g.bytes_per_cluster)
                .max(g.record_size),
        );
        let offset = number * u64::from(g.record_size);
        let e = extents
            .iter()
            .find(|e| (e.vcn * cluster..(e.vcn + e.clusters) * cluster).contains(&offset))
            .ok_or_else(|| self.bad_mft(MftError::Unsupported("MFT record outside its runs")))?;
        let within = offset - e.vcn * cluster;
        let start = within - within % unit;
        #[allow(clippy::cast_possible_truncation)] // a few KiB at most
        let mut buf = vec![0u8; unit as usize];
        self.read_at(e.lcn * cluster + start, &mut buf)?;
        #[allow(clippy::cast_possible_truncation)] // less than `unit`
        let at = (within - start) as usize;
        Ok(buf[at..at + g.record_size as usize].to_vec())
    }

    /// Reads exactly `buf.len()` bytes at byte `offset` of the volume.
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), VolumeError> {
        let mut done = 0;
        while done < buf.len() {
            match self.file.seek_read(&mut buf[done..], offset + done as u64) {
                Ok(0) => return Err(self.error(io::ErrorKind::UnexpectedEof.into())),
                Ok(n) => done += n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(self.error(e)),
            }
        }
        Ok(())
    }

    fn bad_mft(&self, source: MftError) -> VolumeError {
        VolumeError::BadMft {
            letter: self.letter,
            source,
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
    }

    /// `C:` opened with admin rights, or `None` when the test isn't elevated (and that is
    /// allowed: CI sets `MTSEEK_REQUIRE_ADMIN` to turn the skip into a failure).
    fn elevated_c() -> Option<Volume> {
        match Volume::open('C') {
            Ok(v) => Some(v),
            Err(VolumeError::NotElevated) => {
                assert!(
                    std::env::var_os("MTSEEK_REQUIRE_ADMIN").is_none(),
                    "MTSEEK_REQUIRE_ADMIN is set but the test is not elevated"
                );
                None
            }
            Err(e) => panic!("{e}"),
        }
    }

    /// `cmd.exe` is hard-linked into `WinSxS` on every standard install. The raw MFT must
    /// list every one of its links, as `fsutil hardlink list` (no admin needed) does.
    #[test]
    fn raw_mft_lists_every_hard_link_when_elevated() {
        let Some(volume) = elevated_c() else { return };
        let cmd = Path::new(r"C:\Windows\System32\cmd.exe");
        let target = file_ref(cmd).unwrap();
        let listed = std::process::Command::new("fsutil")
            .args(["hardlink", "list"])
            .arg(cmd)
            .output()
            .unwrap();
        let mut expected: Vec<(u64, String)> = String::from_utf8_lossy(&listed.stdout)
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| {
                let path = Path::new(r"C:\").join(l.trim().trim_start_matches('\\'));
                let parent = file_ref(path.parent().unwrap()).unwrap();
                let name = path.file_name().unwrap().to_string_lossy().into_owned();
                (parent, name)
            })
            .collect();
        expected.sort();
        assert!(
            expected.len() >= 2,
            "cmd.exe has {} link(s)",
            expected.len()
        );

        let mut found = Vec::new();
        let stats = volume
            .read_mft(&mut |n| {
                if n.file_ref == target {
                    found.push((n.parent_ref, n.name));
                }
            })
            .unwrap();
        found.sort();
        assert_eq!(found, expected);
        assert!(stats.records > 10_000 && stats.names > 10_000, "{stats:?}");
        assert!(stats.bad_records * 1000 <= stats.records, "{stats:?}");
    }

    /// Every name the enumeration reports must also come from the raw records. The raw
    /// read may add names (other hard links), and the volume changes between the two
    /// reads, so a little slack is allowed for churn.
    #[test]
    fn raw_mft_covers_the_enumeration_when_elevated() {
        use std::collections::HashSet;
        use std::hash::{DefaultHasher, Hash, Hasher};
        let Some(volume) = elevated_c() else { return };
        let key = |file: u64, parent: u64, name: &str| {
            let mut h = DefaultHasher::new();
            (file, parent, name).hash(&mut h);
            h.finish()
        };
        let user_record = |file_ref: u64| file_ref & 0x0000_FFFF_FFFF_FFFF >= 24;

        let mut raw = HashSet::new();
        let stats = volume
            .read_mft(&mut |n| {
                raw.insert(key(n.file_ref, n.parent_ref, &n.name));
            })
            .unwrap();
        let mut enumerated = 0u64;
        let mut missing = 0u64;
        volume
            .enumerate(&mut |r| {
                if user_record(r.file_ref) {
                    enumerated += 1;
                    missing += u64::from(!raw.contains(&key(r.file_ref, r.parent_ref, &r.name)));
                }
            })
            .unwrap();
        eprintln!("raw: {stats:?}; enumeration: {enumerated} names, {missing} not in raw");
        assert!(enumerated > 10_000);
        assert!(
            missing <= 2_000.max(enumerated / 2_000),
            "{missing} of {enumerated} enumerated names are not in the raw MFT"
        );
        assert!(
            stats.names >= enumerated - missing,
            "raw must have at least the enumeration's names"
        );
    }

    #[test]
    fn geometry_is_sane_when_elevated() {
        let Some(volume) = elevated_c() else { return };
        let g = volume.geometry().unwrap();
        assert!(g.record_size == 1024 || g.record_size == 4096, "{g:?}");
        assert!(
            g.bytes_per_cluster.is_power_of_two() && g.bytes_per_sector >= 512,
            "{g:?}"
        );
        assert!(
            g.mft_valid_length >= 1 << 20 && g.mft_start_lcn > 0,
            "{g:?}"
        );
    }
}
