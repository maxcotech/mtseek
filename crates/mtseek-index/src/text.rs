//! Which bytes of a file are stored, and which text the matcher sees in them (ADR 0010).

use std::borrow::Cow;
use std::io::Read;

/// How many leading bytes of `bytes` must be stored for the search to behave exactly as
/// it does on the whole file.
///
/// When binary files are skipped (`include_binary` false), ripgrep's searcher stops at the
/// first NUL and never reads past it, so everything after it can go. A file that starts
/// with a UTF-16 byte order mark is the exception: its NULs are measured after decoding,
/// so it is kept whole.
pub fn stored_len(bytes: &[u8], include_binary: bool) -> usize {
    if include_binary || has_utf16_bom(bytes) {
        return bytes.len();
    }
    memchr_nul(bytes).map_or(bytes.len(), |i| i + 1)
}

/// The text ripgrep's matcher sees when it searches `stored`: the bytes themselves, or the
/// UTF-8 decoding of a UTF-16 file, using grep-searcher's own decoder settings.
pub fn matcher_text(stored: &[u8]) -> Cow<'_, [u8]> {
    if !has_utf16_bom(stored) {
        return Cow::Borrowed(stored);
    }
    let mut decoded = Vec::with_capacity(stored.len());
    let mut reader = encoding_rs_io::DecodeReaderBytesBuilder::new()
        .utf8_passthru(true)
        .strip_bom(true)
        .bom_override(true)
        .bom_sniffing(true)
        .build(stored);
    // Reading from memory can't fail; if decoding somehow did, index the raw bytes too so
    // the trigram set stays a superset.
    if reader.read_to_end(&mut decoded).is_err() {
        decoded.extend_from_slice(stored);
    }
    Cow::Owned(decoded)
}

pub(crate) fn has_utf16_bom(bytes: &[u8]) -> bool {
    bytes.starts_with(&[0xFF, 0xFE]) || bytes.starts_with(&[0xFE, 0xFF])
}

fn memchr_nul(bytes: &[u8]) -> Option<usize> {
    memchr::memchr(0, bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binary_files_are_cut_after_the_first_nul() {
        assert_eq!(stored_len(b"abc\0def\0", false), 4);
        assert_eq!(stored_len(b"\0", false), 1);
        assert_eq!(stored_len(b"abc\0def", true), 7);
        assert_eq!(stored_len(b"no nul", false), 6);
    }

    #[test]
    fn utf16_files_are_kept_whole_and_decoded() {
        let mut bytes = vec![0xFF, 0xFE];
        bytes.extend("hi\u{0}there".encode_utf16().flat_map(u16::to_le_bytes));
        assert_eq!(stored_len(&bytes, false), bytes.len());
        assert_eq!(&*matcher_text(&bytes), "hi\0there".as_bytes());

        let mut be = vec![0xFE, 0xFF];
        be.extend("ok".encode_utf16().flat_map(u16::to_be_bytes));
        assert_eq!(&*matcher_text(&be), b"ok");
    }

    #[test]
    fn other_text_is_seen_as_is() {
        assert!(matches!(matcher_text(b"plain"), Cow::Borrowed(b"plain")));
        // A UTF-8 BOM stays; its trigrams are harmless extras.
        assert_eq!(&*matcher_text(b"\xEF\xBB\xBFx"), b"\xEF\xBB\xBFx");
    }
}
