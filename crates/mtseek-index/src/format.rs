//! The segment file layout (ADR 0010) and the byte-level helpers shared by the writer and
//! the reader.
//!
//! ```text
//! header   "MTSKCIDX" version:u32 0:u32 docs:u32 blocks:u32 trigrams:u32 errors:u32
//! sections scope, docs, paths, errors, dictionary, postings, blocks, store
//! trailer  SECTIONS × (offset:u64, length:u64) "MTSKCEND"
//! ```
//!
//! All integers are little-endian.

use std::path::{Path, PathBuf};

pub(crate) const MAGIC: &[u8; 8] = b"MTSKCIDX";
pub(crate) const END_MAGIC: &[u8; 8] = b"MTSKCEND";
pub(crate) const VERSION: u32 = 1;
pub(crate) const HEADER_LEN: usize = 32;
pub(crate) const SECTIONS: usize = 8;
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
}

/// `block, offset in block, stored length, path offset, path length | UTF16_PATH, root`.
pub(crate) const DOC_LEN: usize = 24;
/// `trigram, file count, postings offset`.
pub(crate) const DICT_LEN: usize = 16;
/// `store offset, compressed length, raw length`.
pub(crate) const BLOCK_LEN: usize = 16;
/// Set in a doc's path length when the path is stored as UTF-16 (not valid Unicode).
pub(crate) const UTF16_PATH: u32 = 1 << 31;
/// The block of an empty file: there is nothing to read.
pub(crate) const NO_BLOCK: u32 = u32::MAX;
/// Uncompressed size a block is filled to before it is compressed. A bigger file gets a
/// block of its own.
pub(crate) const BLOCK_TARGET: usize = 64 * 1024;
/// zstd level for the store (ADR 0010).
pub(crate) const ZSTD_LEVEL: i32 = 3;

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
