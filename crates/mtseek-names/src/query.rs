//! Name search: parallel substring scan over the arena.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use memchr::memmem::Finder;
use mtseek_core::CaseMode;

use crate::{DEAD, DIR, FileId, MAX_DEPTH, NameIndex};

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
        let threads = if threads == 0 {
            std::thread::available_parallelism().map_or(1, std::num::NonZero::get)
        } else {
            threads
        };
        if query.terms.is_empty() {
            return self.all_under(under);
        }
        let arena = if query.case_sensitive {
            &self.names
        } else {
            &self.folded
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
        sort_paths(&mut paths);
        paths
    }

    /// Every live entry inside `under` (default: the root), already in path order.
    ///
    /// Building every path and then sorting them is slow at this scale: a million paths
    /// that share long prefixes. Instead, entries are grouped by parent and each
    /// directory's names are sorted. A depth-first walk from `under` then emits paths in
    /// `Path` order (siblings by name, each directory followed by its contents), extending
    /// one path buffer as it goes. Entries outside `under` are never visited.
    fn all_under(&self, under: Option<FileId>) -> Vec<PathBuf> {
        let top = under.unwrap_or(self.root_id);
        let Some(start_path) = self.dir_path(top) else {
            return Vec::new();
        };
        // The walk extends a `String`; a root that isn't Unicode takes the slow way.
        let Ok(mut path) = start_path.into_os_string().into_string() else {
            return self.all_under_by_sorting(under);
        };
        // Live entries grouped by parent. Sorting compact (parent, slot) pairs keeps this
        // pass in cache; names are sorted later, and only in directories the walk enters,
        // because most of a whole-drive index lies outside any one search root.
        #[allow(clippy::cast_possible_truncation)] // slot counts fit in u32 (len_u32)
        let mut order: Vec<(FileId, u32)> = (0..self.slots.len() as u32)
            .filter(|&i| self.slots[i as usize].flags & DEAD == 0)
            .map(|i| (self.slots[i as usize].parent, i))
            .collect();
        order.sort_unstable();
        let mut children: HashMap<FileId, (usize, usize)> = HashMap::new();
        for (i, &(parent, _)) in order.iter().enumerate() {
            children.entry(parent).or_insert((i, i)).1 = i + 1;
        }
        let enter = |order: &mut [(FileId, u32)], dir: FileId, base: usize| {
            let &(first, end) = children.get(&dir)?;
            order[first..end].sort_unstable_by(|a, b| {
                self.name_bytes(&self.slots[a.1 as usize])
                    .cmp(self.name_bytes(&self.slots[b.1 as usize]))
            });
            Some((first, end, base))
        };

        let mut out = Vec::new();
        // Each directory is entered at most once, so even a corrupt parent cycle ends.
        let mut entered = vec![false; self.slots.len()];
        // One frame per open directory: the next child to emit, the end of its group, and
        // the length of the directory's own path in `path`.
        let mut stack: Vec<(usize, usize, usize)> =
            enter(&mut order, top, path.len()).into_iter().collect();
        while let Some(frame) = stack.last_mut() {
            if frame.0 == frame.1 {
                stack.pop();
                continue;
            }
            let index = order[frame.0].1;
            frame.0 += 1;
            // Join the way `PathBuf::push` does: a separator unless one is already there.
            path.truncate(frame.2);
            if !path.ends_with(std::path::is_separator) {
                path.push(std::path::MAIN_SEPARATOR);
            }
            let slot = &self.slots[index as usize];
            path.push_str(self.name(slot));
            out.push(PathBuf::from(&path));
            // Descend only through a directory's current slot (a moved directory's old
            // slot is dead), and never deeper than any real tree.
            let current = slot.flags & DIR != 0 && self.dirs.get(&slot.id) == Some(&index);
            if current && !entered[index as usize] && stack.len() < MAX_DEPTH {
                entered[index as usize] = true;
                if let Some(frame) = enter(&mut order, slot.id, path.len()) {
                    stack.push(frame);
                }
            }
        }
        out
    }

    /// The same listing, by building every path and sorting. Only for roots whose path
    /// isn't valid Unicode.
    fn all_under_by_sorting(&self, under: Option<FileId>) -> Vec<PathBuf> {
        #[allow(clippy::cast_possible_truncation)] // slot counts fit in u32 (len_u32)
        let mut paths: Vec<PathBuf> = (0..self.slots.len() as u32)
            .filter(|&i| self.slots[i as usize].flags & DEAD == 0)
            .filter_map(|i| self.path_of(i, under))
            .collect();
        sort_paths(&mut paths);
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

/// Sorts paths in `Path`'s own order (component by component), much faster.
///
/// `Path::cmp` re-parses both paths into components on every comparison, which made
/// sorting a million results take 6–10 s. For the paths an index builds (one root, names
/// joined by separators, no `.` or `..`), comparing the raw bytes with every separator
/// counted as lower than any other byte gives the same order: two paths first differ
/// either inside a name, where bytes decide, or where one name ends and the other goes
/// on, where the shorter name (followed by a separator or nothing) comes first.
fn sort_paths(paths: &mut [PathBuf]) {
    paths.sort_unstable_by(|a, b| path_order(a, b));
}

fn path_order(a: &Path, b: &Path) -> std::cmp::Ordering {
    fn key(byte: u8) -> u8 {
        if byte.is_ascii() && std::path::is_separator(char::from(byte)) {
            0
        } else {
            byte
        }
    }
    let (a, b) = (
        a.as_os_str().as_encoded_bytes(),
        b.as_os_str().as_encoded_bytes(),
    );
    // Skip the shared prefix at memcmp speed, then compare the rest with separators as 0.
    let same = a.iter().zip(b).take_while(|(x, y)| x == y).count();
    a[same..]
        .iter()
        .map(|&x| key(x))
        .cmp(b[same..].iter().map(|&x| key(x)))
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
        /// Listing everything walks the tree instead of sorting. It must still give every
        /// live entry under the start, in exactly `Path`'s order, including after moves.
        #[test]
        fn listing_everything_is_complete_and_in_path_order(
            // (parent choice, name, is_dir) for 1..60 entries; parents are earlier dirs.
            entries in proptest::collection::vec(
                (any::<prop::sample::Index>(), "[ !\\-.a-cA_]{0,2}[a-c]", any::<bool>()), 1..60),
            moved in any::<prop::sample::Index>(),
            pick in any::<prop::sample::Index>(),
        ) {
            let mut idx = NameIndex::new(1, if cfg!(windows) { r"C:\r" } else { "/r" });
            let mut dirs = vec![1u64];
            let mut taken = std::collections::HashSet::new();
            for (i, (parent, name, is_dir)) in entries.iter().enumerate() {
                let id = i as u64 + 2;
                let parent = dirs[parent.index(dirs.len())];
                if !taken.insert((parent, name.clone())) {
                    continue; // one folder can't hold two entries with the same name
                }
                idx.insert(id, parent, name, if *is_dir { Kind::Dir } else { Kind::File });
                if *is_dir { dirs.push(id); }
            }
            // Move one directory under the root (its subtree follows; its old slot dies).
            let d = dirs[moved.index(dirs.len())];
            if d != 1 && taken.insert((1, "moved".to_owned())) {
                idx.insert(d, 1, "moved", Kind::Dir);
            }
            let under = dirs[pick.index(dirs.len())];
            let listed = idx.search(&NameQuery::new("", CaseMode::Smart), Some(under), 1);

            // Expected: every live entry's path that lies under `under`, sorted the slow way.
            let mut expected: Vec<PathBuf> = (0..u32::try_from(idx.slots.len()).unwrap())
                .filter(|&i| idx.slots[i as usize].flags & DEAD == 0)
                .filter_map(|i| idx.path_of(i, Some(under)))
                .collect();
            expected.sort();
            prop_assert_eq!(listed, expected);
        }

        /// The fast sort must give exactly `Path`'s order. Names mix in bytes on both
        /// sides of the separator (space, `!`, `-`, `.` sort below `\` and `/`; letters
        /// above), which is where a plain byte sort would differ.
        #[test]
        fn fast_sort_matches_path_order(
            paths in proptest::collection::vec(
                // Real names never end in a space or dot (so are never `.` or `..`).
                proptest::collection::vec("[ !\\-.a-cA_é]{0,3}[a-cé]", 1..5), 0..40),
        ) {
            let mut paths: Vec<PathBuf> = paths
                .iter()
                .map(|names| {
                    let mut p = PathBuf::from(if cfg!(windows) { r"C:\root" } else { "/root" });
                    p.extend(names);
                    p
                })
                .collect();
            let mut expected = paths.clone();
            expected.sort();
            sort_paths(&mut paths);
            prop_assert_eq!(paths, expected);
        }

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
