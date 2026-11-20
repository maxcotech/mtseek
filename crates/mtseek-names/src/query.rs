//! Name search: parallel substring scan over the arena.

use std::path::PathBuf;

use memchr::memmem::Finder;
use mtseek_core::CaseMode;

use crate::{DEAD, FileId, NameIndex};

/// A name search: every whitespace-separated term must occur in the name (like Everything).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NameQuery {
    terms: Vec<String>,
    case_sensitive: bool,
}

impl NameQuery {
    /// Parses `pattern` into terms. `CaseMode::Smart` is case-sensitive only if the pattern
    /// contains an uppercase letter.
    pub fn new(pattern: &str, case: CaseMode) -> Self {
        let case_sensitive = match case {
            CaseMode::Sensitive => true,
            CaseMode::Insensitive => false,
            CaseMode::Smart => pattern.chars().any(char::is_uppercase),
        };
        let terms = pattern
            .split_whitespace()
            .map(|t| {
                if case_sensitive {
                    t.to_owned()
                } else {
                    t.to_lowercase()
                }
            })
            .collect();
        Self {
            terms,
            case_sensitive,
        }
    }
}

impl NameIndex {
    /// All live entries whose name matches `query`, as full paths sorted by path.
    ///
    /// With `under`, only entries inside that directory (at any depth). `threads = 0` uses
    /// every logical CPU.
    pub fn search(&self, query: &NameQuery, under: Option<FileId>, threads: usize) -> Vec<PathBuf> {
        if query.terms.is_empty() {
            return self.all_under(under);
        }
        let arena = if query.case_sensitive {
            &self.names
        } else {
            &self.folded
        };
        let threads = if threads == 0 {
            std::thread::available_parallelism().map_or(1, std::num::NonZero::get)
        } else {
            threads
        };
        let first = query.terms.first().map_or("", String::as_str);
        let finder = Finder::new(first.as_bytes());
        let chunks = split_at_separators(arena, threads);

        let mut paths: Vec<PathBuf> = std::thread::scope(|scope| {
            let workers: Vec<_> = chunks
                .iter()
                .map(|&(start, end)| {
                    let finder = &finder;
                    scope.spawn(move || self.scan(arena, start, end, finder, query, under))
                })
                .collect();
            workers
                .into_iter()
                .flat_map(|w| {
                    w.join()
                        .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
                })
                .collect()
        });
        paths.sort_unstable();
        paths
    }

    /// Every live entry (optionally inside `under`), sorted by path.
    fn all_under(&self, under: Option<FileId>) -> Vec<PathBuf> {
        let mut paths: Vec<PathBuf> = (0..self.slots.len())
            .filter(|&i| self.slots[i].flags & DEAD == 0)
            .filter_map(|i| {
                #[allow(clippy::cast_possible_truncation)] // slot counts fit in u32
                self.path_of(i as u32, under)
            })
            .collect();
        paths.sort_unstable();
        paths
    }

    fn scan(
        &self,
        arena: &[u8],
        start: usize,
        end: usize,
        finder: &Finder<'_>,
        query: &NameQuery,
        under: Option<FileId>,
    ) -> Vec<PathBuf> {
        let mut out = Vec::new();
        let mut last = None;
        for hit in finder.find_iter(&arena[start..end]) {
            let index = self.slot_at(start + hit, query.case_sensitive);
            if last == Some(index) {
                continue; // several hits in one name
            }
            last = Some(index);
            let slot = &self.slots[index];
            if slot.flags & DEAD != 0 {
                continue;
            }
            let name = if query.case_sensitive {
                self.name(slot).as_bytes()
            } else {
                self.folded_name(slot)
            };
            let all_terms = query
                .terms
                .iter()
                .skip(1)
                .all(|t| contains(name, t.as_bytes()));
            if !all_terms {
                continue;
            }
            #[allow(clippy::cast_possible_truncation)] // slot counts fit in u32 (len_u32)
            if let Some(path) = self.path_of(index as u32, under) {
                out.push(path);
            }
        }
        out
    }

    /// The slot whose name contains arena byte `pos`. `pos` is inside a name because the
    /// needle is non-empty and never contains the NUL separator.
    fn slot_at(&self, pos: usize, case_sensitive: bool) -> usize {
        let start = |s: &crate::Slot| if case_sensitive { s.name } else { s.folded } as usize;
        self.slots
            .partition_point(|s| start(s) <= pos)
            .saturating_sub(1)
    }
}

/// Splits `arena` into about `parts` ranges that each start at a NUL separator, so no name
/// straddles two ranges.
fn split_at_separators(arena: &[u8], parts: usize) -> Vec<(usize, usize)> {
    let step = arena.len().div_ceil(parts.max(1)).max(1);
    let mut ranges = Vec::new();
    let mut start = 0;
    while start < arena.len() {
        let mut end = (start + step).min(arena.len());
        if let Some(sep) = memchr::memchr(0, &arena[end..]) {
            end += sep;
        } else {
            end = arena.len();
        }
        ranges.push((start, end));
        start = end;
    }
    ranges
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    memchr::memmem::find(haystack, needle).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Kind, tests::sample};
    use proptest::prelude::*;
    use std::path::Path;

    fn names(paths: &[PathBuf]) -> Vec<String> {
        paths
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn smart_case_substring_search() {
        let idx = sample();
        let q = NameQuery::new("pay", CaseMode::Smart);
        let found = names(&idx.search(&q, None, 0));
        assert_eq!(found, vec!["PayGYM", "PaymentService.cs", "payment.log"]);
        let q = NameQuery::new("Pay", CaseMode::Smart);
        assert_eq!(
            names(&idx.search(&q, None, 0)),
            vec!["PayGYM", "PaymentService.cs"]
        );
    }

    #[test]
    fn every_term_must_match() {
        let idx = sample();
        let q = NameQuery::new("pay .cs", CaseMode::Smart);
        assert_eq!(names(&idx.search(&q, None, 0)), vec!["PaymentService.cs"]);
    }

    #[test]
    fn under_limits_results_to_a_subtree() {
        let idx = sample();
        let q = NameQuery::new("pay", CaseMode::Smart);
        let found = idx.search(&q, Some(10), 0);
        assert_eq!(
            found,
            vec![
                PathBuf::from(r"C:\Dev\PayGYM"),
                PathBuf::from(r"C:\Dev\PayGYM\PaymentService.cs")
            ]
        );
        assert!(
            idx.search(&q, Some(11), 0)
                .iter()
                .all(|p| p.starts_with(r"C:\Dev\PayGYM\"))
        );
    }

    #[test]
    fn unicode_case_folding() {
        let mut idx = NameIndex::new(0, "/r");
        idx.insert(1, 0, "ÉCOLE.txt", Kind::File);
        let q = NameQuery::new("école", CaseMode::Smart);
        assert_eq!(
            idx.search(&q, None, 0),
            vec![Path::new("/r").join("ÉCOLE.txt")]
        );
    }

    #[test]
    fn empty_query_lists_everything_under() {
        let idx = sample();
        let q = NameQuery::new("", CaseMode::Smart);
        assert_eq!(idx.search(&q, Some(11), 0).len(), 2);
    }

    proptest! {
        /// The arena scan must agree with a naive filter over every live entry, for any
        /// set of names, any query and any thread count.
        #[test]
        fn matches_naive_search(
            names in proptest::collection::vec("[a-cA-C.]{1,6}", 1..60),
            removed in proptest::collection::vec(any::<prop::sample::Index>(), 0..10),
            pattern in "[a-cA-C.]{0,3}( [a-c]{1,2})?",
            threads in 1usize..5,
        ) {
            let mut idx = NameIndex::new(0, "/r");
            for (i, name) in names.iter().enumerate() {
                idx.insert(i as u64 + 1, 0, name, Kind::File);
            }
            let mut live: Vec<(u64, &String)> =
                names.iter().enumerate().map(|(i, n)| (i as u64 + 1, n)).collect();
            for r in removed {
                let (id, _) = live[r.index(live.len())];
                live.retain(|(i, _)| *i != id);
                if live.is_empty() { break; }
            }
            // Removal happens by re-listing the directory without the removed names.
            let listing = live
                .iter()
                .map(|(id, n)| crate::Child { id: *id, name: (*n).clone(), kind: Kind::File })
                .collect();
            idx.replace_children(vec![(0, listing)]);
            let q = NameQuery::new(&pattern, CaseMode::Smart);
            let mut expected: Vec<PathBuf> = live
                .iter()
                .filter(|(_, n)| {
                    let n = if q.case_sensitive { (*n).clone() } else { n.to_lowercase() };
                    q.terms.iter().all(|t| n.contains(t.as_str()))
                })
                .map(|(_, n)| Path::new("/r").join(n))
                .collect();
            expected.sort_unstable();
            prop_assert_eq!(idx.search(&q, None, threads), expected);
        }
    }
}
