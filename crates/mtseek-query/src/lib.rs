//! Content search through the index (ADR 0001, 0010, 0011).
//!
//! 1. [`plan()`] turns the pattern into a trigram query.
//! 2. The query runs against the postings of each layer of the index (base and delta);
//!    base files the delta replaced are skipped, and so are files outside the searched
//!    folder or not listed by the search's filter (ADR 0012). The result is the candidate
//!    files.
//! 3. Each candidate's stored bytes are searched by the reference scanner's own
//!    [`FileSearcher`], so what matches, and how lines and binary files are reported, is
//!    exactly what `mtseek-walk` (and ripgrep) would report.
//!
//! Invariant 2 (index = oracle) is checked by property tests in `tests/oracle.rs`.

mod plan;

use std::collections::HashMap;
use std::ops::ControlFlow;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Mutex, PoisonError};

use mtseek_core::{Error, Filter, Query, Result};
use mtseek_index::{Index, IndexError, Location, Segment, membership};
use mtseek_walk::{FileHit, FileSearcher, Mode, Options, Summary};

pub use plan::{Plan, plan};

/// What a search did, for `--stats`.
#[derive(Debug, Clone)]
pub struct SearchStats {
    /// The trigram query.
    pub plan: Plan,
    /// Files whose bytes were searched.
    pub candidates: u64,
    /// Store blocks decompressed.
    pub blocks: u64,
}

/// What a search of an index looks at (ADR 0012).
#[derive(Debug, Clone, Copy)]
pub struct Target<'a> {
    /// The search root as the user gave it. Results are joined onto it, exactly as the
    /// scanner would join them.
    pub root: &'a Path,
    /// The folder of the index that `root` names, relative to the index's root: empty for the
    /// root itself. The walk that built the index must enter it with the search's filter
    /// ([`Index::entered`]).
    pub dir: &'a Path,
    /// The search's filter. It must list no more than the index's coverage filter does
    /// ([`membership::covered`]) and must not follow links.
    pub filter: Filter,
}

/// Searches the files of `index` under `target` for `query`, the way [`mtseek_walk::scan`]
/// searches a scope with root `target.root` and filter `target.filter`.
///
/// Files that couldn't be read are reported to `on_error`. An error reading the index
/// itself ends the search with that error: the caller should rebuild the index or fall back
/// to scanning.
pub fn search(
    index: &Index,
    target: Target<'_>,
    query: &Query,
    mode: Mode,
    options: Options,
    on_hit: &(dyn Fn(&FileHit<'_>) -> ControlFlow<()> + Sync),
    on_error: &(dyn Fn(&Error) + Sync),
) -> Result<(Summary, SearchStats)> {
    let matcher = mtseek_walk::build_matcher(query)?;
    if index.base().roots() != 1 {
        return Err(Error::Io {
            path: target.root.to_path_buf(),
            source: std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "a content index covers one root",
            ),
        });
    }
    let bit = membership::bit(&target.filter);
    let plan = plan::plan(query);
    let layers = index.layers();
    let mut per_layer = Vec::with_capacity(layers.len());
    for &(segment, skip) in &layers {
        let (lo, hi) = range_under(segment, target.dir);
        let mut found: Vec<u32> = match candidates(&plan, segment)? {
            Some(list) => list.into_iter().filter(|&d| d >= lo && d < hi).collect(),
            None => (lo..hi).collect(),
        };
        if target.filter.include_binary {
            // The trigrams of a cut file stop at its first NUL, but binary mode reads on: it
            // is a candidate whatever the plan says, and is read from disk.
            let cut: Vec<u32> = (lo..hi)
                .filter(|&d| segment.flags(d) & membership::TRUNCATED != 0)
                .collect();
            found = union(&found, &cut);
        }
        let mut found = difference(&found, skip);
        found.retain(|&d| segment.flags(d) & bit != 0);
        per_layer.push(found);
    }
    let candidates: u64 = per_layer.iter().map(|c| c.len() as u64).sum();

    let errors = AtomicU64::new(0);
    for stored in index.errors() {
        if stored.flags & bit == 0 {
            continue;
        }
        let path = match stored.root {
            Some(_) => match stored.path.strip_prefix(target.dir) {
                Ok(rest) => target.root.join(rest),
                Err(_) => continue,
            },
            None => stored.path.clone(),
        };
        errors.fetch_add(1, Ordering::Relaxed);
        on_error(&Error::Io {
            path,
            source: std::io::Error::other(stored.message.clone()),
        });
    }

    let work = group(&layers, &per_layer);

    let next = AtomicUsize::new(0);
    let stop = AtomicBool::new(false);
    let files_matched = AtomicU64::new(0);
    let blocks = AtomicU64::new(0);
    let failure: Mutex<Option<IndexError>> = Mutex::new(None);
    let threads = options.thread_count().clamp(1, work.len().max(1));
    std::thread::scope(|threads_scope| {
        for _ in 0..threads {
            threads_scope.spawn(|| {
                let worker = Worker {
                    layers: &layers,
                    target,
                    on_hit,
                    on_error,
                    stop: &stop,
                    matched: &files_matched,
                    errors: &errors,
                };
                let result = worker.run(&matcher, mode, &work, &next, &blocks);
                if let Err(e) = result {
                    stop.store(true, Ordering::Relaxed);
                    failure
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .get_or_insert(e);
                }
            });
        }
    });
    if let Some(e) = failure.into_inner().unwrap_or_else(PoisonError::into_inner) {
        return Err(e.into());
    }
    Ok((
        Summary {
            files_searched: candidates,
            files_matched: files_matched.into_inner(),
            errors: errors.into_inner(),
        },
        SearchStats {
            plan,
            candidates,
            blocks: blocks.into_inner(),
        },
    ))
}

/// Splits each layer's candidates into units of work: consecutive candidates in one store
/// block are decompressed once; files outside the store go one by one.
fn group<'a>(layers: &[(&Segment, &[u32])], per_layer: &'a [Vec<u32>]) -> Vec<Work<'a>> {
    let mut work = Vec::new();
    for (layer, (&(segment, _), docs)) in layers.iter().zip(per_layer).enumerate() {
        let mut rest = &docs[..];
        while let Some(&first) = rest.first() {
            let block = match segment.location(first) {
                Location::Block { block, .. } => Some(block),
                Location::Empty | Location::External => None,
            };
            let n = match block {
                Some(b) => rest
                    .iter()
                    .take_while(|&&d| {
                        matches!(segment.location(d), Location::Block { block, .. } if block == b)
                    })
                    .count(),
                None => 1,
            };
            let (head, tail) = rest.split_at(n);
            work.push(Work {
                layer,
                block,
                docs: head,
            });
            rest = tail;
        }
    }
    work
}

/// Candidates of one layer that share a store block (or one file outside the store).
#[derive(Clone, Copy)]
struct Work<'a> {
    layer: usize,
    block: Option<u32>,
    docs: &'a [u32],
}

struct Worker<'a> {
    layers: &'a [(&'a Segment, &'a [u32])],
    target: Target<'a>,
    on_hit: &'a (dyn Fn(&FileHit<'_>) -> ControlFlow<()> + Sync),
    on_error: &'a (dyn Fn(&Error) + Sync),
    stop: &'a AtomicBool,
    matched: &'a AtomicU64,
    errors: &'a AtomicU64,
}

impl Worker<'_> {
    fn run(
        &self,
        matcher: &mtseek_walk::RegexMatcher,
        mode: Mode,
        work: &[Work<'_>],
        next: &AtomicUsize,
        blocks: &AtomicU64,
    ) -> Result<(), IndexError> {
        let mut readers = Vec::with_capacity(self.layers.len());
        for (segment, _) in self.layers {
            readers.push(segment.block_reader()?);
        }
        let mut searcher = FileSearcher::new(self.target.filter, mode);
        // In binary mode ripgrep reads past the first NUL, which the store cut off.
        let from_disk = |doc: u32, segment: &Segment| {
            self.target.filter.include_binary && segment.flags(doc) & membership::TRUNCATED != 0
        };
        while !self.stop.load(Ordering::Relaxed) {
            let Some(&item) = work.get(next.fetch_add(1, Ordering::Relaxed)) else {
                break;
            };
            let segment = self.layers[item.layer].0;
            let raw: &[u8] = match item.block {
                Some(b) => {
                    blocks.fetch_add(1, Ordering::Relaxed);
                    readers[item.layer].read(b)?
                }
                None => &[],
            };
            for &doc in item.docs {
                let (_, relative) = segment.path(doc);
                let path = match relative.strip_prefix(self.target.dir) {
                    Ok(rest) => self.target.root.join(rest),
                    Err(_) => continue, // outside the folder: excluded by the range already
                };
                let found = match segment.location(doc) {
                    _ if from_disk(doc, segment) => searcher.search_path(matcher, &path),
                    Location::Block { offset, len, .. } => {
                        let bytes = &raw[offset as usize..(offset + len) as usize];
                        searcher.search_bytes(matcher, bytes)
                    }
                    Location::Empty => searcher.search_bytes(matcher, &[]),
                    Location::External => searcher.search_path(matcher, &path),
                };
                let found = match found {
                    Ok(Some(found)) => found,
                    Ok(None) => continue,
                    Err(source) => {
                        self.errors.fetch_add(1, Ordering::Relaxed);
                        (self.on_error)(&Error::Io { path, source });
                        continue;
                    }
                };
                self.matched.fetch_add(1, Ordering::Relaxed);
                let hit = FileHit {
                    path: &path,
                    lines: found.lines,
                    binary_offset: found.binary_offset,
                };
                if (self.on_hit)(&hit).is_break() {
                    self.stop.store(true, Ordering::Relaxed);
                    return Ok(());
                }
            }
        }
        Ok(())
    }
}

/// The files of `segment` under folder `dir` (relative to its root): a range of ids, because
/// files are sorted by path and a folder's files are contiguous in that order.
fn range_under(segment: &Segment, dir: &Path) -> (u32, u32) {
    if dir.as_os_str().is_empty() {
        return (0, segment.len());
    }
    let first = |pred: &dyn Fn(&Path) -> bool| -> u32 {
        // The first id for which `pred` is false; `pred` holds for a prefix of the ids.
        let (mut lo, mut hi) = (0u32, segment.len());
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if pred(&segment.path(mid).1) {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        lo
    };
    let lo = first(&|p| p < dir);
    let hi = first(&|p| p < dir || p.starts_with(dir));
    (lo, hi)
}

/// `a` without the elements of `b` (both increasing).
fn difference(a: &[u32], b: &[u32]) -> Vec<u32> {
    if b.is_empty() {
        return a.to_vec();
    }
    let mut out = Vec::with_capacity(a.len());
    let mut j = 0;
    for &x in a {
        while j < b.len() && b[j] < x {
            j += 1;
        }
        if b.get(j) != Some(&x) {
            out.push(x);
        }
    }
    out
}

/// The files satisfying `plan`, in increasing order, or `None` for all of them.
fn candidates(plan: &Plan, segment: &Segment) -> Result<Option<Vec<u32>>, IndexError> {
    Evaluator {
        segment,
        postings: HashMap::new(),
    }
    .eval(plan)
}

struct Evaluator<'a> {
    segment: &'a Segment,
    postings: HashMap<u32, Vec<u32>>,
}

impl Evaluator<'_> {
    fn eval(&mut self, plan: &Plan) -> Result<Option<Vec<u32>>, IndexError> {
        Ok(match plan {
            Plan::All => None,
            Plan::Nothing => Some(Vec::new()),
            Plan::Trigram(key) => Some(self.postings(*key)?),
            Plan::And(items) => {
                // Cheapest first, so an empty intersection is found early.
                let mut items: Vec<&Plan> = items.iter().collect();
                items.sort_by_key(|p| match p {
                    Plan::Trigram(k) => u64::from(self.segment.count(*k)),
                    _ => u64::MAX,
                });
                let mut acc: Option<Vec<u32>> = None;
                for item in items {
                    if let Some(list) = self.eval(item)? {
                        acc = Some(match acc {
                            None => list,
                            Some(a) => intersect(&a, &list),
                        });
                    }
                    if acc.as_ref().is_some_and(Vec::is_empty) {
                        break;
                    }
                }
                acc
            }
            Plan::Or(items) => {
                let mut acc = Vec::new();
                for item in items {
                    match self.eval(item)? {
                        None => return Ok(None),
                        Some(list) => acc = union(&acc, &list),
                    }
                }
                Some(acc)
            }
        })
    }

    fn postings(&mut self, key: u32) -> Result<Vec<u32>, IndexError> {
        if let Some(list) = self.postings.get(&key) {
            return Ok(list.clone());
        }
        let list = self.segment.postings(key)?;
        self.postings.insert(key, list.clone());
        Ok(list)
    }
}

fn intersect(a: &[u32], b: &[u32]) -> Vec<u32> {
    let (mut i, mut j) = (0, 0);
    let mut out = Vec::with_capacity(a.len().min(b.len()));
    while i < a.len() && j < b.len() {
        match a[i].cmp(&b[j]) {
            std::cmp::Ordering::Less => i += 1,
            std::cmp::Ordering::Greater => j += 1,
            std::cmp::Ordering::Equal => {
                out.push(a[i]);
                i += 1;
                j += 1;
            }
        }
    }
    out
}

fn union(a: &[u32], b: &[u32]) -> Vec<u32> {
    let (mut i, mut j) = (0, 0);
    let mut out = Vec::with_capacity(a.len() + b.len());
    while i < a.len() && j < b.len() {
        match a[i].cmp(&b[j]) {
            std::cmp::Ordering::Less => {
                out.push(a[i]);
                i += 1;
            }
            std::cmp::Ordering::Greater => {
                out.push(b[j]);
                j += 1;
            }
            std::cmp::Ordering::Equal => {
                out.push(a[i]);
                i += 1;
                j += 1;
            }
        }
    }
    out.extend_from_slice(&a[i..]);
    out.extend_from_slice(&b[j..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        #[test]
        fn merges_match_sets(a in proptest::collection::btree_set(0u32..64, 0..30), b in proptest::collection::btree_set(0u32..64, 0..30)) {
            let av: Vec<u32> = a.iter().copied().collect();
            let bv: Vec<u32> = b.iter().copied().collect();
            prop_assert_eq!(intersect(&av, &bv), a.intersection(&b).copied().collect::<Vec<_>>());
            prop_assert_eq!(union(&av, &bv), a.union(&b).copied().collect::<Vec<_>>());
            prop_assert_eq!(difference(&av, &bv), a.difference(&b).copied().collect::<Vec<_>>());
        }
    }
}
