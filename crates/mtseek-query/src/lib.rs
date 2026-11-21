//! Content search through the index (ADR 0001, 0010, 0011).
//!
//! 1. [`plan()`] turns the pattern into a trigram query.
//! 2. The query runs against the postings of each layer of the index (base and delta);
//!    base files the delta replaced are skipped. The result is the candidate files.
//! 3. Each candidate's stored bytes are searched by the reference scanner's own
//!    [`FileSearcher`], so what matches, and how lines and binary files are reported, is
//!    exactly what `mtseek-walk` (and ripgrep) would report.
//!
//! Invariant 2 (index = oracle) is checked by property tests in `tests/oracle.rs`.

mod plan;

use std::collections::HashMap;
use std::ops::ControlFlow;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Mutex, PoisonError};

use mtseek_core::{Error, Query, Result, Scope};
use mtseek_index::{Index, IndexError, Location, Segment};
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

/// Searches the files of `index` for `query`, the way [`mtseek_walk::scan`] searches the
/// files of `scope`.
///
/// `scope` must be the scope the index was built for (same roots in the same order, same
/// filter; compare [`mtseek_index::scope_key`] with [`Segment::key`]). Paths are reported
/// joined onto `scope.roots` as given, exactly like the scanner reports them.
///
/// Files that couldn't be read are reported to `on_error`. An error reading the index
/// itself ends the search with that error: the caller should rebuild the index or fall back
/// to scanning.
pub fn search(
    index: &Index,
    scope: &Scope,
    query: &Query,
    mode: Mode,
    options: Options,
    on_hit: &(dyn Fn(&FileHit<'_>) -> ControlFlow<()> + Sync),
    on_error: &(dyn Fn(&Error) + Sync),
) -> Result<(Summary, SearchStats)> {
    let matcher = mtseek_walk::build_matcher(query)?;
    if index.base().roots() != scope.roots.len() {
        return Err(Error::Io {
            path: scope.roots.first().cloned().unwrap_or_default(),
            source: std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "the content index was built for different roots",
            ),
        });
    }
    let plan = plan::plan(query);
    let layers = index.layers();
    let mut per_layer = Vec::with_capacity(layers.len());
    for &(segment, skip) in &layers {
        let found = match candidates(&plan, segment)? {
            Some(list) => list,
            None => (0..segment.len()).collect(),
        };
        per_layer.push(difference(&found, skip));
    }
    let candidates: u64 = per_layer.iter().map(|c| c.len() as u64).sum();

    let errors = AtomicU64::new(0);
    for stored in index.errors() {
        let path = match stored.root {
            Some(root) => scope.roots[root as usize].join(&stored.path),
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
                    scope,
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
    scope: &'a Scope,
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
        let mut searcher = FileSearcher::new(self.scope.filter, mode);
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
                let (root, relative) = segment.path(doc);
                let path = self.scope.roots[root].join(relative);
                let found = match segment.location(doc) {
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

/// Where the segment for `scope` lives in `cache_dir`, and the key it must carry.
pub fn segment_path(
    cache_dir: &std::path::Path,
    scope: &Scope,
) -> std::io::Result<(PathBuf, String)> {
    let key = mtseek_index::scope_key(scope)?;
    let hash = key.bytes().fold(0xcbf2_9ce4_8422_2325_u64, |h, b| {
        (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
    });
    Ok((cache_dir.join(format!("content-{hash:016x}.idx")), key))
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
