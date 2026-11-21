//! Directory listing with file reference numbers.
//!
//! `std::fs::read_dir` doesn't expose a file's NTFS identity. This lister reads
//! `FILE_ID_BOTH_DIR_INFO` records (the same `NtQueryDirectoryFile` call that `FindFirstFile`
//! makes), so every entry comes back with its file reference number at no extra cost. Those
//! numbers are what the USN journal reports, which is how a saved name index is matched
//! against later changes.
//!
//! The record parser is portable and never panics on bad input. Opening and reading a
//! directory is Windows-only.

/// `FILE_ATTRIBUTE_DIRECTORY`.
const ATTRIBUTE_DIRECTORY: u32 = 0x10;
/// `FILE_ATTRIBUTE_REPARSE_POINT`.
const ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
/// Bit 29 of a reparse tag: the reparse point stands for another name (symlink, junction).
const TAG_NAME_SURROGATE: u32 = 0x2000_0000;

// FILE_ID_BOTH_DIR_INFO field offsets (checked against windows-sys in the tests).
const NEXT_ENTRY_OFFSET: usize = 0;
const FILE_ATTRIBUTES: usize = 56;
const FILE_NAME_LENGTH: usize = 60;
const EA_SIZE: usize = 64;
const FILE_ID: usize = 96;
const FILE_NAME: usize = 104;

/// One entry of a directory listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirEntry {
    /// The file reference number. Zero on file systems that have none (some FAT drivers).
    pub file_ref: u64,
    /// `FILE_ATTRIBUTE_*` flags.
    pub attributes: u32,
    /// For reparse points, the reparse tag; otherwise zero.
    pub reparse_tag: u32,
    /// The file name (not the path), with unpaired surrogates replaced by U+FFFD.
    pub name: String,
}

impl DirEntry {
    /// Whether this is a directory to descend into. Symbolic links and junctions to
    /// directories are not: they stand for another path, and following them could loop.
    /// That matches `std::fs::FileType::is_dir` and ripgrep's default.
    pub fn is_dir(&self) -> bool {
        self.attributes & ATTRIBUTE_DIRECTORY != 0
            && !(self.attributes & ATTRIBUTE_REPARSE_POINT != 0
                && self.reparse_tag & TAG_NAME_SURROGATE != 0)
    }
}

/// Parses a buffer of chained `FILE_ID_BOTH_DIR_INFO` records into `out`, skipping `.` and
/// `..`. Returns `false` if the buffer was malformed (entries parsed before that are kept).
pub fn parse_dir_info(buf: &[u8], out: &mut Vec<DirEntry>) -> bool {
    let mut at = 0usize;
    loop {
        let Some(entry) = buf.get(at..) else {
            return false;
        };
        if entry.len() < FILE_NAME {
            return false;
        }
        let next = u32_at(entry, NEXT_ENTRY_OFFSET) as usize;
        let name_len = u32_at(entry, FILE_NAME_LENGTH) as usize;
        let Some(name_bytes) = entry
            .get(FILE_NAME..FILE_NAME.saturating_add(name_len))
            .filter(|b| b.len().is_multiple_of(2))
        else {
            return false;
        };
        let (pairs, _) = name_bytes.as_chunks::<2>();
        let units: Vec<u16> = pairs.iter().map(|&p| u16::from_le_bytes(p)).collect();
        let is_dot = units == [u16::from(b'.')] || units == [u16::from(b'.'), u16::from(b'.')];
        if !is_dot {
            let attributes = u32_at(entry, FILE_ATTRIBUTES);
            out.push(DirEntry {
                file_ref: u64_at(entry, FILE_ID),
                attributes,
                reparse_tag: if attributes & ATTRIBUTE_REPARSE_POINT == 0 {
                    0
                } else {
                    u32_at(entry, EA_SIZE)
                },
                name: String::from_utf16_lossy(&units),
            });
        }
        if next == 0 {
            return true;
        }
        if next < FILE_NAME {
            return false; // would not advance past this entry's fixed part
        }
        at = at.saturating_add(next);
    }
}

fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

fn u64_at(b: &[u8], at: usize) -> u64 {
    let mut w = [0; 8];
    w.copy_from_slice(&b[at..at + 8]);
    u64::from_le_bytes(w)
}

#[cfg(windows)]
pub use windows::{DirBuffer, Directory};

#[cfg(windows)]
mod windows {
    use std::ffi::OsString;
    use std::io;
    use std::os::windows::ffi::{OsStrExt, OsStringExt};
    use std::os::windows::io::{AsHandle, OwnedHandle};
    use std::path::{Component, Path, Prefix};

    use windows_sys::Win32::Storage::FileSystem::{
        FILE_FLAG_BACKUP_SEMANTICS, FILE_LIST_DIRECTORY,
    };

    use super::{DirEntry, parse_dir_info};
    use crate::sys;

    /// A reusable, 8-byte-aligned buffer for [`Directory::read`]. One per thread.
    #[derive(Debug)]
    pub struct DirBuffer(Vec<u64>);

    impl Default for DirBuffer {
        fn default() -> Self {
            Self(vec![0; 64 * 1024 / 8]) // 64 KiB holds ~500 entries per call
        }
    }

    /// An open directory.
    #[derive(Debug)]
    pub struct Directory {
        handle: OwnedHandle,
    }

    impl Directory {
        /// Opens a directory for listing. Long paths are fine.
        pub fn open(path: &Path) -> io::Result<Self> {
            let handle = sys::open(
                &verbatim(path),
                FILE_LIST_DIRECTORY,
                FILE_FLAG_BACKUP_SEMANTICS,
            )?;
            Ok(Self { handle })
        }

        /// The directory's own file reference number.
        pub fn file_ref(&self) -> io::Result<u64> {
            sys::file_index(self.handle.as_handle())
        }

        /// Appends every entry except `.` and `..` to `out`.
        pub fn read(&self, buf: &mut DirBuffer, out: &mut Vec<DirEntry>) -> io::Result<()> {
            let mut restart = true;
            while let Some(bytes) = sys::query_directory(&self.handle, &mut buf.0, restart)? {
                restart = false;
                if !parse_dir_info(bytes, out) {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "malformed directory listing",
                    ));
                }
            }
            Ok(())
        }
    }

    /// Win32 calls reject paths longer than `MAX_PATH` unless they use the verbatim
    /// `\\?\` form, which skips normalisation. mtseek's paths are already absolute and
    /// normalised, so long ones are converted. Short ones pass through unchanged.
    fn verbatim(path: &Path) -> OsString {
        const LIMIT: usize = 240; // a little under MAX_PATH (260)
        let mut components = path.components();
        let (Some(Component::Prefix(prefix)), Some(Component::RootDir)) =
            (components.next(), components.next())
        else {
            return path.into();
        };
        if path.as_os_str().len() < LIMIT {
            return path.into();
        }
        let wide: Vec<u16> = path.as_os_str().encode_wide().collect();
        let long: Vec<u16> = match prefix.kind() {
            // C:\x -> \\?\C:\x
            Prefix::Disk(_) => r"\\?\".encode_utf16().chain(wide).collect(),
            // \\server\share\x -> \\?\UNC\server\share\x
            Prefix::UNC(..) => r"\\?\UNC"
                .encode_utf16()
                .chain(wide.into_iter().skip(1))
                .collect(),
            _ => return path.into(),
        };
        OsString::from_wide(&long)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn long_paths_become_verbatim() {
            let long = "x".repeat(250);
            assert_eq!(verbatim(Path::new(r"C:\short")), r"C:\short");
            assert_eq!(
                verbatim(&Path::new(r"C:\").join(&long)),
                OsString::from(format!(r"\\?\C:\{long}"))
            );
            assert_eq!(
                verbatim(&Path::new(r"\\srv\share\").join(&long)),
                OsString::from(format!(r"\\?\UNC\srv\share\{long}"))
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    /// Builds one `FILE_ID_BOTH_DIR_INFO` record; `next` is filled in by `chain`.
    fn record(file_ref: u64, attributes: u32, tag: u32, name: &str) -> Vec<u8> {
        let units: Vec<u16> = name.encode_utf16().collect();
        let mut r = vec![0u8; FILE_NAME + units.len() * 2];
        r[FILE_ATTRIBUTES..FILE_ATTRIBUTES + 4].copy_from_slice(&attributes.to_le_bytes());
        r[FILE_NAME_LENGTH..FILE_NAME_LENGTH + 4]
            .copy_from_slice(&u32::try_from(units.len() * 2).unwrap().to_le_bytes());
        r[EA_SIZE..EA_SIZE + 4].copy_from_slice(&tag.to_le_bytes());
        r[FILE_ID..FILE_ID + 8].copy_from_slice(&file_ref.to_le_bytes());
        for (i, u) in units.iter().enumerate() {
            r[FILE_NAME + i * 2..FILE_NAME + i * 2 + 2].copy_from_slice(&u.to_le_bytes());
        }
        while !r.len().is_multiple_of(8) {
            r.push(0);
        }
        r
    }

    fn chain(records: &[Vec<u8>]) -> Vec<u8> {
        let mut buf = Vec::new();
        for (i, r) in records.iter().enumerate() {
            let mut r = r.clone();
            if i + 1 < records.len() {
                let next = u32::try_from(r.len()).unwrap();
                r[0..4].copy_from_slice(&next.to_le_bytes());
            }
            buf.extend_from_slice(&r);
        }
        buf
    }

    #[test]
    fn parses_entries_and_skips_dots() {
        let buf = chain(&[
            record(1, 0x10, 0, "."),
            record(2, 0x10, 0, ".."),
            record(7, 0x10, 0, "src"),
            record(8, 0x20, 0, "Cargo.toml"),
            record(9, 0x10 | 0x400, 0xA000_0003, "junction"), // IO_REPARSE_TAG_MOUNT_POINT
            record(10, 0x10 | 0x400, 0x9000_601A, "OneDrive"), // cloud placeholder
        ]);
        let mut out = Vec::new();
        assert!(parse_dir_info(&buf, &mut out));
        let summary: Vec<(u64, &str, bool)> = out
            .iter()
            .map(|e| (e.file_ref, e.name.as_str(), e.is_dir()))
            .collect();
        assert_eq!(
            summary,
            [
                (7, "src", true),
                (8, "Cargo.toml", false),
                (9, "junction", false),
                (10, "OneDrive", true),
            ]
        );
    }

    #[test]
    fn rejects_truncated_and_looping_buffers() {
        let one = record(7, 0x20, 0, "file.txt");
        let mut out = Vec::new();
        assert!(!parse_dir_info(&one[..FILE_NAME + 2], &mut out));
        let mut looping = one.clone();
        looping[0..4].copy_from_slice(&4u32.to_le_bytes());
        assert!(!parse_dir_info(&looping, &mut Vec::new()));
    }

    #[cfg(windows)]
    #[test]
    fn offsets_match_the_windows_sdk() {
        use std::mem::offset_of;
        use windows_sys::Win32::Storage::FileSystem::FILE_ID_BOTH_DIR_INFO as Info;
        assert_eq!(offset_of!(Info, NextEntryOffset), NEXT_ENTRY_OFFSET);
        assert_eq!(offset_of!(Info, FileAttributes), FILE_ATTRIBUTES);
        assert_eq!(offset_of!(Info, FileNameLength), FILE_NAME_LENGTH);
        assert_eq!(offset_of!(Info, EaSize), EA_SIZE);
        assert_eq!(offset_of!(Info, FileId), FILE_ID);
        assert_eq!(offset_of!(Info, FileName), FILE_NAME);
    }

    #[cfg(windows)]
    #[test]
    fn lists_a_real_directory_with_file_refs() {
        use std::path::Path;
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("a.txt"), "a").unwrap();
        // A long path, beyond MAX_PATH, must list too.
        let mut deep = dir.path().to_path_buf();
        while deep.as_os_str().len() < 300 {
            deep.push("a-fairly-long-directory-name");
        }
        std::fs::create_dir_all(&deep).unwrap();
        std::fs::write(deep.join("deep.txt"), "").unwrap();

        let listed = |path: &Path| {
            let d = Directory::open(path).unwrap();
            let mut out = Vec::new();
            d.read(&mut DirBuffer::default(), &mut out).unwrap();
            out.sort_by(|a, b| a.name.cmp(&b.name));
            (d.file_ref().unwrap(), out)
        };
        let (own_ref, entries) = listed(dir.path());
        assert_eq!(own_ref, crate::file_ref(dir.path()).unwrap());
        let names: Vec<_> = entries
            .iter()
            .map(|e| (e.name.as_str(), e.is_dir()))
            .collect();
        assert_eq!(
            names,
            [
                ("a-fairly-long-directory-name", true),
                ("a.txt", false),
                ("sub", true)
            ]
        );
        assert_eq!(
            entries[1].file_ref,
            crate::file_ref(&dir.path().join("a.txt")).unwrap()
        );
        let (_, deep_entries) = listed(&deep);
        assert_eq!(deep_entries[0].name, "deep.txt");
    }

    proptest! {
        #[test]
        fn garbage_never_panics(bytes in proptest::collection::vec(any::<u8>(), 0..400)) {
            let _ = parse_dir_info(&bytes, &mut Vec::new());
        }

        #[test]
        fn roundtrips_names(names in proptest::collection::vec("[^./\\\\]{1,20}", 1..20)) {
            let records: Vec<_> = names
                .iter()
                .enumerate()
                .map(|(i, n)| record(i as u64 + 1, 0x20, 0, n))
                .collect();
            let mut out = Vec::new();
            prop_assert!(parse_dir_info(&chain(&records), &mut out));
            let got: Vec<&str> = out.iter().map(|e| e.name.as_str()).collect();
            prop_assert_eq!(got, names.iter().map(String::as_str).collect::<Vec<_>>());
        }
    }
}
