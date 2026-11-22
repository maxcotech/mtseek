//! The segment file layout (ADR 0010, 0011) and the byte-level helpers shared by the writer
//! and the reader.
//!
//! ```text
//! header   "MTSKCIDX" version:u32 roots:u32 docs:u32 blocks:u32 trigrams:u32 errors:u32
//! sections scope, docs, paths, errors, dictionary, postings, blocks, store,
//!          dirs, controls, tombstones, meta, descriptors
//! trailer  SECTIONS × (offset:u64, length:u64) "MTSKCEND"
//! ```
//!
//! All integers are little-endian.

use std::path::{Path, PathBuf};

use crate::{Checkpoint, Entry, Meta, Sd};

pub(crate) const MAGIC: &[u8; 8] = b"MTSKCIDX";
pub(crate) const END_MAGIC: &[u8; 8] = b"MTSKCEND";
pub(crate) const VERSION: u32 = 5;
pub(crate) const HEADER_LEN: usize = 32;
pub(crate) const SECTIONS: usize = 13;
pub(crate) const TRAILER_LEN: usize = SECTIONS * 16 + 8;

/// Section numbers, in file order.
pub(crate) mod section {
    pub(crate) const SCOPE: usize = 0;
    pub(crate) const DOCS: usize = 1;
    pub(crate) const PATHS: usize = 2;
    pub(crate) const ERRORS: usize = 3;
    pub(crate) const DICTIONARY: usize = 4;
    pub(crate) const POSTINGS: usize = 5;
    pub(crate) const BLOCKS: usize = 6;
    pub(crate) const STORE: usize = 7;
    /// Directories the walk entered: [`super::put_entry`] records. Read only by catch-up.
    pub(crate) const DIRS: usize = 8;
    /// Ignore-control files (`.gitignore`, ...): [`super::put_entry`] records.
    pub(crate) const CONTROLS: usize = 9;
    /// In a delta: the base's files it replaces or deletes, increasing `u32` ids.
    pub(crate) const TOMBSTONES: usize = 10;
    /// Build id, base id and journal checkpoints ([`super::put_meta`]).
    pub(crate) const META: usize = 11;
    /// The distinct security descriptors that files, directories and control files refer to
    /// by number ([`super::put_descriptors`], ADR 0014).
    pub(crate) const DESCRIPTORS: usize = 12;

    /// Sections that are read only on demand, so may be big.
    pub(crate) fn is_lazy(i: usize) -> bool {
        matches!(i, POSTINGS | STORE | DIRS | CONTROLS)
    }
}

/// `block, offset in block, stored length, path offset, path length | UTF16_PATH, root,
/// file reference number (u64), flags` ([`crate::membership`]), descriptor number.
pub(crate) const DOC_LEN: usize = 40;
/// `trigram, file count, postings offset`.
pub(crate) const DICT_LEN: usize = 16;
/// `store offset, compressed length, raw length`.
pub(crate) const BLOCK_LEN: usize = 16;
/// Set in a path length when the path is stored as UTF-16 (not valid Unicode).
pub(crate) const UTF16_PATH: u32 = 1 << 31;
/// The block of an empty file: there is nothing to read.
pub(crate) const NO_BLOCK: u32 = u32::MAX;
/// Uncompressed size a block is filled to before it is compressed. A bigger file gets a
/// block of its own.
pub(crate) const BLOCK_TARGET: usize = 64 * 1024;
/// zstd level for the store (ADR 0010).
pub(crate) const ZSTD_LEVEL: i32 = 3;

/// Appends one dirs/controls record:
/// `frn:u64 root:u32 flags:u32 descriptor:u32 path_len:u32 path`.
pub(crate) fn put_entry(out: &mut Vec<u8>, entry: &Entry, descriptor: u32) {
    let (path, utf16) = encode_path(&entry.path);
    // Paths are far shorter than 2 GiB; an absurd one is cut rather than corrupting the file.
    let len = u32::try_from(path.len()).unwrap_or(0).min(UTF16_PATH - 1);
    out.extend_from_slice(&entry.frn.to_le_bytes());
    out.extend_from_slice(&entry.root.to_le_bytes());
    out.extend_from_slice(&u32::from(entry.flags).to_le_bytes());
    out.extend_from_slice(&descriptor.to_le_bytes());
    out.extend_from_slice(&(len | if utf16 { UTF16_PATH } else { 0 }).to_le_bytes());
    out.extend_from_slice(&path[..len as usize]);
}

/// Parses a dirs/controls section. `None` if it is malformed, names a root `>= roots` or a
/// descriptor not in `descriptors`.
pub(crate) fn parse_entries(
    mut bytes: &[u8],
    roots: u32,
    descriptors: &[Sd],
) -> Option<Vec<Entry>> {
    let mut out = Vec::new();
    while !bytes.is_empty() {
        let frn = u64_at(take(&mut bytes, 8)?, 0);
        let root = u32_at(take(&mut bytes, 4)?, 0);
        let flags = u8::try_from(u32_at(take(&mut bytes, 4)?, 0)).ok()?;
        let sd = descriptors
            .get(u32_at(take(&mut bytes, 4)?, 0) as usize)?
            .clone();
        let len = u32_at(take(&mut bytes, 4)?, 0);
        let path = decode_path(
            take(&mut bytes, (len & !UTF16_PATH) as usize)?,
            len & UTF16_PATH != 0,
        )?;
        if root >= roots {
            return None;
        }
        out.push(Entry {
            frn,
            root,
            path,
            flags,
            sd,
        });
    }
    Some(out)
}

/// Encodes the descriptors section: `n:u32`, then `n` times `len:u32 bytes`.
pub(crate) fn put_descriptors(table: &[Sd]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&u32::try_from(table.len()).unwrap_or(0).to_le_bytes());
    for sd in table {
        out.extend_from_slice(&u32::try_from(sd.len()).unwrap_or(0).to_le_bytes());
        out.extend_from_slice(sd);
    }
    out
}

/// Parses the descriptors section. `None` if it is malformed. A descriptor longer than
/// 64 KiB is malformed: Windows limits them to that.
pub(crate) fn parse_descriptors(mut bytes: &[u8]) -> Option<Vec<Sd>> {
    let n = u32_at(take(&mut bytes, 4)?, 0);
    let mut out = Vec::new();
    for _ in 0..n {
        let len = u32_at(take(&mut bytes, 4)?, 0) as usize;
        if len > 1 << 16 {
            return None;
        }
        out.push(Sd::from(take(&mut bytes, len)?));
    }
    bytes.is_empty().then_some(out)
}

/// Encodes the meta section: `build_id:u64 base_id:u64 n:u32` then `n` checkpoints of
/// `letter:u32 journal_id:u64 usn:i64`.
pub(crate) fn put_meta(meta: &Meta) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&meta.build_id.to_le_bytes());
    out.extend_from_slice(&meta.base_id.to_le_bytes());
    let n = u32::try_from(meta.checkpoints.len()).unwrap_or(0);
    out.extend_from_slice(&n.to_le_bytes());
    for c in meta.checkpoints.iter().take(n as usize) {
        out.extend_from_slice(&u32::from(c.letter).to_le_bytes());
        out.extend_from_slice(&c.journal_id.to_le_bytes());
        out.extend_from_slice(&c.usn.to_le_bytes());
    }
    out
}

/// Parses the meta section. `None` if it is malformed.
pub(crate) fn parse_meta(mut bytes: &[u8]) -> Option<Meta> {
    let build_id = u64_at(take(&mut bytes, 8)?, 0);
    let base_id = u64_at(take(&mut bytes, 8)?, 0);
    let n = u32_at(take(&mut bytes, 4)?, 0);
    let mut checkpoints = Vec::new();
    for _ in 0..n {
        let letter = char::from_u32(u32_at(take(&mut bytes, 4)?, 0))?;
        let journal_id = u64_at(take(&mut bytes, 8)?, 0);
        let usn = i64::from_le_bytes(take(&mut bytes, 8)?.try_into().ok()?);
        checkpoints.push(Checkpoint {
            letter,
            journal_id,
            usn,
        });
    }
    bytes.is_empty().then_some(Meta {
        build_id,
        base_id,
        checkpoints,
    })
}

/// Splits `n` bytes off the front of `bytes`.
pub(crate) fn take<'a>(bytes: &mut &'a [u8], n: usize) -> Option<&'a [u8]> {
    let (head, rest) = bytes.split_at_checked(n)?;
    *bytes = rest;
    Some(head)
}

/// Encodes a relative path for the paths section. Returns the bytes and whether they are
/// UTF-16 (only for names that aren't valid Unicode, which Windows allows).
pub(crate) fn encode_path(path: &Path) -> (Vec<u8>, bool) {
    if let Some(s) = path.to_str() {
        return (s.as_bytes().to_vec(), false);
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        let bytes = path
            .as_os_str()
            .encode_wide()
            .flat_map(u16::to_le_bytes)
            .collect();
        (bytes, true)
    }
    #[cfg(not(windows))]
    {
        use std::os::unix::ffi::OsStrExt;
        (path.as_os_str().as_bytes().to_vec(), true)
    }
}

/// Reverses [`encode_path`]. `None` if the bytes can't be a path of that kind.
pub(crate) fn decode_path(bytes: &[u8], utf16: bool) -> Option<PathBuf> {
    if !utf16 {
        return std::str::from_utf8(bytes).ok().map(PathBuf::from);
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStringExt;
        let (pairs, rest) = bytes.as_chunks::<2>();
        if !rest.is_empty() {
            return None;
        }
        let wide: Vec<u16> = pairs.iter().map(|&c| u16::from_le_bytes(c)).collect();
        Some(std::ffi::OsString::from_wide(&wide).into())
    }
    #[cfg(not(windows))]
    {
        use std::os::unix::ffi::OsStringExt;
        Some(std::ffi::OsString::from_vec(bytes.to_vec()).into())
    }
}

/// Appends `value` as a LEB128 varint.
pub(crate) fn put_varint(out: &mut Vec<u8>, mut value: u32) {
    while value >= 0x80 {
        out.push(low_byte(value) | 0x80);
        value >>= 7;
    }
    out.push(low_byte(value));
}

#[allow(clippy::cast_possible_truncation)] // keeping only the low byte is the point
fn low_byte(value: u32) -> u8 {
    value as u8
}

/// Reads a LEB128 varint at `*pos`, advancing it. `None` if truncated or too long.
pub(crate) fn get_varint(bytes: &[u8], pos: &mut usize) -> Option<u32> {
    let mut value = 0u32;
    for shift in (0..35).step_by(7) {
        let b = *bytes.get(*pos)?;
        *pos += 1;
        let bits = u32::from(b & 0x7F);
        if shift == 28 && bits > 0x0F {
            return None; // would overflow u32
        }
        value |= bits << shift;
        if b & 0x80 == 0 {
            return Some(value);
        }
    }
    None
}

/// The little-endian `u32` at byte `at`. Callers check bounds when the segment is opened.
pub(crate) fn u32_at(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap_or([0; 4]))
}

/// The little-endian `u64` at byte `at`. Callers check bounds when the segment is opened.
pub(crate) fn u64_at(bytes: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap_or([0; 8]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        #[test]
        fn varints_round_trip(values in proptest::collection::vec(any::<u32>(), 0..50)) {
            let mut bytes = Vec::new();
            for &v in &values {
                put_varint(&mut bytes, v);
            }
            let mut pos = 0;
            for &v in &values {
                prop_assert_eq!(get_varint(&bytes, &mut pos), Some(v));
            }
            prop_assert_eq!(pos, bytes.len());
        }

        #[test]
        fn varint_reader_never_panics(bytes in proptest::collection::vec(any::<u8>(), 0..12)) {
            let mut pos = 0;
            while get_varint(&bytes, &mut pos).is_some() {}
        }
    }

    #[test]
    fn entries_and_meta_round_trip() {
        let entries = vec![
            Entry {
                frn: 7,
                root: 0,
                path: PathBuf::from("a\\b"),
                flags: 5,
                sd: Sd::from(&b"one"[..]),
            },
            Entry {
                frn: u64::MAX,
                root: 1,
                path: PathBuf::new(),
                flags: 0,
                sd: Sd::from(&b"two"[..]),
            },
        ];
        let table = vec![Sd::from(&b"one"[..]), Sd::from(&b"two"[..])];
        let mut bytes = Vec::new();
        for (i, e) in (0u32..).zip(&entries) {
            put_entry(&mut bytes, e, i);
        }
        assert_eq!(parse_entries(&bytes, 2, &table).unwrap(), entries);
        assert!(
            parse_entries(&bytes, 1, &table).is_none(),
            "root out of range"
        );
        assert!(
            parse_entries(&bytes, 2, &table[..1]).is_none(),
            "descriptor out of range"
        );
        assert!(
            parse_entries(&bytes[..bytes.len() - 1], 2, &table).is_none(),
            "truncated"
        );
        let encoded = put_descriptors(&table);
        assert_eq!(parse_descriptors(&encoded).unwrap(), table);
        assert!(parse_descriptors(&encoded[..encoded.len() - 1]).is_none());

        let meta = Meta {
            build_id: 1,
            base_id: 2,
            checkpoints: vec![Checkpoint {
                letter: 'C',
                journal_id: 3,
                usn: -4,
            }],
        };
        let bytes = put_meta(&meta);
        assert_eq!(parse_meta(&bytes).unwrap(), meta);
        assert!(parse_meta(&bytes[..bytes.len() - 1]).is_none());
    }

    #[test]
    fn paths_round_trip() {
        let p = Path::new("dir\\sub\\file.txt");
        let (bytes, utf16) = encode_path(p);
        assert!(!utf16);
        assert_eq!(decode_path(&bytes, utf16).unwrap(), p);
    }

    #[cfg(windows)]
    #[test]
    fn non_unicode_paths_round_trip() {
        use std::os::windows::ffi::OsStringExt;
        let p = PathBuf::from(std::ffi::OsString::from_wide(&[0x61, 0xD800, 0x62]));
        let (bytes, utf16) = encode_path(&p);
        assert!(utf16);
        assert_eq!(decode_path(&bytes, utf16).unwrap(), p);
        assert!(decode_path(&bytes[1..], true).is_none());
    }
}
