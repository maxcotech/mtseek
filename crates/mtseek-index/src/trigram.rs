//! Trigram keys: three ASCII-lowercased bytes packed into a `u32`.
//!
//! The index and the query planner both use these functions, so they always agree on what
//! a trigram is.

/// Folds a byte the way the index does: ASCII letters to lowercase, everything else as is.
#[inline]
pub fn fold(b: u8) -> u8 {
    b.to_ascii_lowercase()
}

/// The key of the trigram `a b c` (folded).
#[inline]
pub fn key(a: u8, b: u8, c: u8) -> u32 {
    (u32::from(fold(a)) << 16) | (u32::from(fold(b)) << 8) | u32::from(fold(c))
}

/// Replaces `out` with the distinct trigram keys of `text`, in increasing order.
pub fn distinct(text: &[u8], out: &mut Vec<u32>) {
    out.clear();
    out.extend(text.windows(3).map(|w| key(w[0], w[1], w[2])));
    out.sort_unstable();
    out.dedup();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_are_case_folded_for_ascii_only() {
        assert_eq!(key(b'A', b'b', b'C'), key(b'a', b'B', b'c'));
        assert_ne!(key(0xC9, b'a', b'a'), key(0xE9, b'a', b'a'));
    }

    #[test]
    fn distinct_trigrams_are_sorted_and_unique() {
        let mut out = vec![99];
        distinct(b"abcabcABC", &mut out);
        let expected = {
            let mut v = vec![
                key(b'a', b'b', b'c'),
                key(b'b', b'c', b'a'),
                key(b'c', b'a', b'b'),
            ];
            v.sort_unstable();
            v
        };
        assert_eq!(out, expected);
        distinct(b"ab", &mut out);
        assert_eq!(out, []);
    }
}
