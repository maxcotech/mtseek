//! Safe parsing of `USN_RECORD_V2` / `USN_RECORD_V3` buffers.
//!
//! Both `FSCTL_ENUM_USN_DATA` (list every file) and `FSCTL_READ_USN_JOURNAL` (list changes)
//! return a buffer of variable-length records. This module turns those bytes into
//! [`UsnRecord`]s. It never panics on bad input: every length and offset is checked.
//!
//! Layout references: `USN_RECORD_V2` and `USN_RECORD_V3` in the Windows SDK (`winioctl.h`).

/// Bits of [`UsnRecord::reason`] that mtseek acts on. The full list is in `winioctl.h`.
pub mod reason {
    /// The file or directory was created.
    pub const FILE_CREATE: u32 = 0x0000_0100;
    /// The file or directory was deleted.
    pub const FILE_DELETE: u32 = 0x0000_0200;
    /// The record carries the name *before* a rename.
    pub const RENAME_OLD_NAME: u32 = 0x0000_1000;
    /// The record carries the name *after* a rename.
    pub const RENAME_NEW_NAME: u32 = 0x0000_2000;
    /// A hard link to the file was added or removed.
    pub const HARD_LINK_CHANGE: u32 = 0x0001_0000;
    /// A reparse point (symbolic link, junction) was added, changed or removed.
    pub const REPARSE_POINT_CHANGE: u32 = 0x0010_0000;
    /// The file's data changed (overwrite, extend or truncate).
    pub const DATA_CHANGED: u32 = 0x0000_0001 | 0x0000_0002 | 0x0000_0004;
    /// The last handle to the file was closed; the record summarises all changes since open.
    pub const CLOSE: u32 = 0x8000_0000;
}

/// `FILE_ATTRIBUTE_DIRECTORY`.
const ATTRIBUTE_DIRECTORY: u32 = 0x10;
/// `FILE_ATTRIBUTE_HIDDEN`.
const ATTRIBUTE_HIDDEN: u32 = 0x02;
/// `FILE_ATTRIBUTE_SYSTEM`.
const ATTRIBUTE_SYSTEM: u32 = 0x04;

/// One file-system entry (from MFT enumeration) or one change (from the journal).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsnRecord {
    /// The file's reference number (MFT record number + sequence number).
    pub file_ref: u64,
    /// The reference number of the directory that contains it.
    pub parent_ref: u64,
    /// Journal position of this change. Zero for records from MFT enumeration.
    pub usn: i64,
    /// When the change happened, as a Windows `FILETIME` (100 ns ticks since 1601, UTC).
    pub timestamp: i64,
    /// What changed: a bit set of [`reason`] flags. Zero for records from MFT enumeration.
    pub reason: u32,
    /// `FILE_ATTRIBUTE_*` flags.
    pub attributes: u32,
    /// The file name (not the path).
    ///
    /// NTFS names are UTF-16 and may contain unpaired surrogates, which are replaced with
    /// U+FFFD here. Such names are vanishingly rare; if one matters, it needs a WTF-8 arena.
    pub name: String,
}

impl UsnRecord {
    /// Whether the entry is a directory.
    pub fn is_dir(&self) -> bool {
        self.attributes & ATTRIBUTE_DIRECTORY != 0
    }

    /// Whether the entry has the hidden or system attribute.
    pub fn is_hidden(&self) -> bool {
        self.attributes & (ATTRIBUTE_HIDDEN | ATTRIBUTE_SYSTEM) != 0
    }

    /// Encodes the record as a `USN_RECORD_V2`, padded to 8 bytes like NTFS does.
    ///
    /// For tests and tools that need realistic buffers without a real volume.
    pub fn to_v2_bytes(&self) -> Vec<u8> {
        let name: Vec<u8> = self
            .name
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect();
        let unpadded = V2_NAME_OFFSET + name.len();
        let len = unpadded.next_multiple_of(8);
        let mut out = Vec::with_capacity(len);
        out.extend_from_slice(&u32::try_from(len).unwrap_or(u32::MAX).to_le_bytes());
        out.extend_from_slice(&2u16.to_le_bytes()); // major version
        out.extend_from_slice(&0u16.to_le_bytes()); // minor version
        out.extend_from_slice(&self.file_ref.to_le_bytes());
        out.extend_from_slice(&self.parent_ref.to_le_bytes());
        out.extend_from_slice(&self.usn.to_le_bytes());
        out.extend_from_slice(&self.timestamp.to_le_bytes());
        out.extend_from_slice(&self.reason.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes()); // source info
        out.extend_from_slice(&0u32.to_le_bytes()); // security id
        out.extend_from_slice(&self.attributes.to_le_bytes());
        out.extend_from_slice(&u16::try_from(name.len()).unwrap_or(u16::MAX).to_le_bytes());
        #[allow(clippy::cast_possible_truncation)] // V2_NAME_OFFSET is 60
        out.extend_from_slice(&(V2_NAME_OFFSET as u16).to_le_bytes());
        out.extend_from_slice(&name);
        out.resize(len, 0);
        out
    }
}

/// Why a buffer could not be parsed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RecordError {
    /// A record claims to be longer than the bytes left, or shorter than its own header.
    #[error("record at byte {offset} has invalid length {length}")]
    BadLength {
        /// Byte offset of the record in the buffer.
        offset: usize,
        /// The length the record declared.
        length: usize,
    },
    /// The record's file name lies outside the record.
    #[error("record at byte {offset} has its name outside the record")]
    BadName {
        /// Byte offset of the record in the buffer.
        offset: usize,
    },
    /// A record version mtseek does not read (V4 range records, or a future version).
    #[error("record at byte {offset} has unsupported version {major}")]
    UnsupportedVersion {
        /// Byte offset of the record in the buffer.
        offset: usize,
        /// The record's major version.
        major: u16,
    },
    /// A V3 record whose 128-bit file ID does not fit in 64 bits (`ReFS`). NTFS IDs always fit.
    #[error("record at byte {offset} has a 128-bit file ID (not NTFS)")]
    WideFileId {
        /// Byte offset of the record in the buffer.
        offset: usize,
    },
}

const V2_NAME_OFFSET: usize = 60;
const V2_MIN_LEN: usize = V2_NAME_OFFSET;
const V3_MIN_LEN: usize = 76;

/// Iterator over the records in a buffer. Stops at the first malformed record.
#[derive(Debug, Clone)]
pub struct Records<'a> {
    buf: &'a [u8],
    offset: usize,
    failed: bool,
}

impl<'a> Records<'a> {
    /// Iterates the records in `buf`: the output of an FSCTL *after* its leading 8-byte
    /// "next position" value.
    pub fn new(buf: &'a [u8]) -> Self {
        Self {
            buf,
            offset: 0,
            failed: false,
        }
    }
}

impl Iterator for Records<'_> {
    type Item = Result<UsnRecord, RecordError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.failed {
            return None;
        }
        let rest = &self.buf[self.offset..];
        if rest.len() < 4 {
            return None;
        }
        let result = parse_one(rest, self.offset);
        match &result {
            Ok((_, len)) => self.offset += len,
            Err(_) => self.failed = true,
        }
        Some(result.map(|(record, _)| record))
    }
}

/// Parses the record at the start of `rest`. Returns it and its length in bytes.
fn parse_one(rest: &[u8], offset: usize) -> Result<(UsnRecord, usize), RecordError> {
    let length = u32_at(rest, 0) as usize; // the iterator guarantees 4 bytes
    if rest.len() < 8 {
        return Err(RecordError::BadLength { offset, length });
    }
    let major = u16_at(rest, 4);
    let min_len = match major {
        2 => V2_MIN_LEN,
        3 => V3_MIN_LEN,
        _ if length >= 8 && length <= rest.len() => {
            return Err(RecordError::UnsupportedVersion { offset, major });
        }
        _ => V2_MIN_LEN,
    };
    if length < min_len || length > rest.len() {
        return Err(RecordError::BadLength { offset, length });
    }
    let record = &rest[..length];

    let (file_ref, parent_ref, base) = if major == 2 {
        (u64_at(record, 8), u64_at(record, 16), 24)
    } else {
        // V3: 16-byte FILE_ID_128s. NTFS puts the 64-bit reference in the low half.
        if u64_at(record, 16) != 0 || u64_at(record, 32) != 0 {
            return Err(RecordError::WideFileId { offset });
        }
        (u64_at(record, 8), u64_at(record, 24), 40)
    };
    let usn = i64_at(record, base);
    let timestamp = i64_at(record, base + 8);
    let reason = u32_at(record, base + 16);
    // base + 20 source info, base + 24 security id.
    let attributes = u32_at(record, base + 28);
    let name_len = u16_at(record, base + 32) as usize;
    let name_offset = u16_at(record, base + 34) as usize;

    let name_bytes = record
        .get(name_offset..name_offset + name_len)
        .filter(|b| b.len() % 2 == 0)
        .ok_or(RecordError::BadName { offset })?;
    let (pairs, _) = name_bytes.as_chunks::<2>(); // even length, checked above
    let units: Vec<u16> = pairs.iter().map(|&pair| u16::from_le_bytes(pair)).collect();
    let name = String::from_utf16_lossy(&units);

    Ok((
        UsnRecord {
            file_ref,
            parent_ref,
            usn,
            timestamp,
            reason,
            attributes,
            name,
        },
        length,
    ))
}

// Callers guarantee bounds via the length checks above.
fn u16_at(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([b[at], b[at + 1]])
}
fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}
fn u64_at(b: &[u8], at: usize) -> u64 {
    let mut bytes = [0; 8];
    bytes.copy_from_slice(&b[at..at + 8]);
    u64::from_le_bytes(bytes)
}
fn i64_at(b: &[u8], at: usize) -> i64 {
    let mut bytes = [0; 8];
    bytes.copy_from_slice(&b[at..at + 8]);
    i64::from_le_bytes(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn record(name: &str) -> UsnRecord {
        UsnRecord {
            file_ref: 0x0003_0000_0000_1234,
            parent_ref: 0x0005_0000_0000_0005,
            usn: 4096,
            timestamp: 133_000_000_000_000_000,
            reason: reason::FILE_CREATE | reason::CLOSE,
            attributes: ATTRIBUTE_DIRECTORY,
            name: name.into(),
        }
    }

    #[test]
    fn parses_a_buffer_of_records() {
        let (a, b) = (record("Program Files"), record("naïve 日本.txt"));
        let mut buf = a.to_v2_bytes();
        buf.extend(b.to_v2_bytes());
        let parsed: Vec<_> = Records::new(&buf).collect::<Result<_, _>>().unwrap();
        assert_eq!(parsed, vec![a.clone(), b]);
        assert!(parsed[0].is_dir());
    }

    #[test]
    fn parses_v3_records_with_64_bit_ids() {
        let r = record("v3.txt");
        let name: Vec<u8> = r.name.encode_utf16().flat_map(u16::to_le_bytes).collect();
        let len = (V3_MIN_LEN + name.len()).next_multiple_of(8);
        let mut buf = Vec::new();
        buf.extend_from_slice(&u32::try_from(len).unwrap().to_le_bytes());
        buf.extend_from_slice(&3u16.to_le_bytes());
        buf.extend_from_slice(&0u16.to_le_bytes());
        buf.extend_from_slice(&r.file_ref.to_le_bytes());
        buf.extend_from_slice(&0u64.to_le_bytes());
        buf.extend_from_slice(&r.parent_ref.to_le_bytes());
        buf.extend_from_slice(&0u64.to_le_bytes());
        buf.extend_from_slice(&r.usn.to_le_bytes());
        buf.extend_from_slice(&r.timestamp.to_le_bytes());
        buf.extend_from_slice(&r.reason.to_le_bytes());
        buf.extend_from_slice(&[0; 8]);
        buf.extend_from_slice(&r.attributes.to_le_bytes());
        buf.extend_from_slice(&u16::try_from(name.len()).unwrap().to_le_bytes());
        buf.extend_from_slice(&u16::try_from(V3_MIN_LEN).unwrap().to_le_bytes());
        buf.extend_from_slice(&name);
        buf.resize(len, 0);
        let parsed: Vec<_> = Records::new(&buf).collect::<Result<_, _>>().unwrap();
        assert_eq!(parsed, vec![r]);
    }

    #[test]
    fn rejects_truncated_and_oversized_records() {
        let mut buf = record("x").to_v2_bytes();
        buf.truncate(buf.len() - 4);
        assert!(matches!(
            Records::new(&buf).next(),
            Some(Err(RecordError::BadLength { .. }))
        ));

        let mut buf = record("x").to_v2_bytes();
        buf[0..4].copy_from_slice(&10u32.to_le_bytes()); // shorter than the header
        assert!(matches!(
            Records::new(&buf).next(),
            Some(Err(RecordError::BadLength { .. }))
        ));
    }

    #[test]
    fn rejects_names_outside_the_record() {
        let mut buf = record("abc").to_v2_bytes();
        buf[56..58].copy_from_slice(&200u16.to_le_bytes());
        assert!(matches!(
            Records::new(&buf).next(),
            Some(Err(RecordError::BadName { .. }))
        ));
    }

    #[test]
    fn stops_after_the_first_error() {
        let mut buf = record("ok").to_v2_bytes();
        buf.extend_from_slice(&[0xFF; 16]);
        let items: Vec<_> = Records::new(&buf).collect();
        assert_eq!(items.len(), 2);
        assert!(items[0].is_ok() && items[1].is_err());
    }

    proptest! {
        #[test]
        fn roundtrips_any_record(
            file_ref: u64, parent_ref: u64, usn: i64, timestamp: i64, reason: u32, attributes: u32,
            name in "\\PC{0,40}",
        ) {
            let r = UsnRecord { file_ref, parent_ref, usn, timestamp, reason, attributes, name };
            let parsed: Vec<_> = Records::new(&r.to_v2_bytes()).collect::<Result<_, _>>().unwrap();
            prop_assert_eq!(parsed, vec![r]);
        }

        #[test]
        fn never_panics_on_garbage(bytes in proptest::collection::vec(any::<u8>(), 0..512)) {
            for item in Records::new(&bytes) {
                let _ = item;
            }
        }
    }
}
