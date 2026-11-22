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
//! Before any of that, the search works out what its caller may see (ADR 0014): folders it
//! may list and files it may read, by their recorded security descriptors. Everything else
//! is reported as `rg` run by the caller would report it ("Access is denied") and never
//! reaches planning, verification or the summary.
//!
//! Glob and file-type rules (`-g`, `-t`) decide which files are listed, the way the walker
//! applies them ([`Rules`]). They can list ignored and hidden files, so an index answers
//! them only when it holds those files ([`unanswerable`], ADR 0015).
//!
//! Invariant 2 (index = oracle) is checked by property tests in `tests/oracle.rs`.

mod plan;

use std::collections::{HashMap, HashSet};
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use mtseek_core::{Error, Filter, Query, Result};
use mtseek_index::{Index, IndexError, Location, Segment, membership};
use mtseek_walk::{
    FileHit, FileSearcher, Known, Options, Output, Rules, Summary, Verdict, Widening,
};

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

/// Who is searching, as far as the index can tell (ADR 0014): which recorded security
/// descriptors let them read a file or list a folder.
pub trait Viewer: Sync {
    /// Whether the caller may open a file with descriptor `sd` for reading.
    fn may_read(&self, sd: &[u8]) -> bool;
    /// Whether the caller may list a folder with descriptor `sd`.
    fn may_list(&self, sd: &[u8]) -> bool;
}

/// A viewer that may read and list everything. For tests, and for systems that record no
/// descriptors.
#[derive(Debug, Clone, Copy, Default)]
pub struct Everyone;

impl Viewer for Everyone {
    fn may_read(&self, _: &[u8]) -> bool {
        true
    }
    fn may_list(&self, _: &[u8]) -> bool {
        true
    }
}

/// The message `rg` prints for a file or folder its account may not open.
fn access_denied(path: PathBuf) -> Error {
    Error::Io {
        path,
        source: std::io::Error::from_raw_os_error(5),
    }
}

/// What a search of an index looks at (ADR 0012).
#[derive(Debug, Clone, Copy)]
pub struct Target<'a> {
    /// The search root as the user gave it. Results are joined onto it, exactly as the
    /// scanner would join them.
    pub root: &'a Path,
    /// Where `root` is on this machine, for reading files from disk: `root` itself, unless
    /// the caller spelled it in a namespace this process doesn't share (another working
    /// directory, a drive mapped in another session).
    pub disk_root: &'a Path,
    /// The folder of the index that `root` names, relative to the index's root: empty for the
    /// root itself. The walk that built the index must enter it with the search's filter
    /// ([`Index::entered`]).
    pub dir: &'a Path,
    /// The search's filter. It must list no more than the index's coverage filter does
    /// ([`membership::covered`]) and must not follow links.
    pub filter: Filter,
}

/// Searches the files of `index` under `target` for `query`, the way [`mtseek_walk::scan`]
/// searches a scope with root `target.root`, filter `target.filter` and `rules`.
///
/// Files that couldn't be read are reported to `on_error`. An error reading the index
/// itself ends the search with that error: the caller should rebuild the index or fall back
/// to scanning. A search the index can't answer exactly ([`unanswerable`]) is
/// [`Error::Declined`] before anything is reported.
#[allow(clippy::too_many_arguments)] // one search's inputs; a struct would only rename them
pub fn search(
    index: &Index,
    target: Target<'_>,
    rules: &Rules,
    query: &Query,
    output: Output,
    options: Options,
    viewer: &dyn Viewer,
    on_hit: &(dyn Fn(&FileHit<'_>) -> ControlFlow<()> + Sync),
    on_error: &(dyn Fn(&Error) + Sync),
) -> Result<(Summary, SearchStats)> {
    let matcher = mtseek_walk::build_matcher(query)?;
    answers(index, target, rules, viewer)?;
    let bit = membership::bit(&target.filter);
    let plan = plan::plan(query);
    let layers = index.layers();
    let errors = AtomicU64::new(0);
    let report_error = |err: Error| {
        errors.fetch_add(1, Ordering::Relaxed);
        on_error(&err);
    };
    let spell = |relative: &Path| match relative.strip_prefix(target.dir) {
        Ok(rest) if rest.as_os_str().is_empty() => Some(target.root.to_path_buf()),
        Ok(rest) => Some(target.root.join(rest)),
        Err(_) => None,
    };
    // Folders the caller may not list: reported once each, and nothing beneath them is.
    let hidden = unlisted(index, target, bit, viewer)?;
    for dir in &hidden {
        if let Some(path) = spell(dir) {
            report_error(access_denied(path));
        }
    }
    let mut listing = Listing::new(index, target, rules)?;
    let mut per_layer = Vec::with_capacity(layers.len());
    for &(segment, skip) in &layers {
        let range = range_under(segment, target.dir);
        let view = LayerView::new(segment, &hidden, viewer);
        for d in view.unreadable(segment, skip, range, &mut |d| listing.lists(segment, d)) {
            if let Some(path) = spell(&segment.path(d).1) {
                report_error(access_denied(path));
            }
        }
        let found = layer_candidates(&plan, segment, range, target.filter.include_binary)?;
        let mut found = difference(&found, skip);
        found.retain(|&d| listing.lists(segment, d) && view.visible(segment, d));
        per_layer.push(found);
    }
    let candidates: u64 = per_layer.iter().map(|c| c.len() as u64).sum();

    for err in stored_errors(index, target, bit, &hidden) {
        report_error(err);
    }

    let work = group(&layers, &per_layer);

    let next = AtomicUsize::new(0);
    let stop = AtomicBool::new(false);
    let files_matched = AtomicU64::new(0);
    let bytes_searched = AtomicU64::new(0);
    let search_nanos = AtomicU64::new(0);
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
                    bytes: &bytes_searched,
                    nanos: &search_nanos,
                };
                let result = worker.run(&matcher, output, &work, &next, &blocks);
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
            bytes_searched: bytes_searched.into_inner(),
            search_time: Duration::from_nanos(search_nanos.into_inner()),
        },
        SearchStats {
            plan,
            candidates,
            blocks: blocks.into_inner(),
        },
    ))
}

/// Fails unless `index` can answer a search of `target` with `rules` exactly.
fn answers(index: &Index, target: Target<'_>, rules: &Rules, viewer: &dyn Viewer) -> Result<()> {
    if index.base().roots() != 1 {
        return Err(Error::Io {
            path: target.root.to_path_buf(),
            source: std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "a content index covers one root",
            ),
        });
    }
    match unanswerable(index, target, rules, viewer)? {
        Some(why) => Err(Error::Declined(why.to_owned())),
        None => Ok(()),
    }
}

/// Why the index can't answer a search of `target` with `rules` exactly, if it can't
/// (ADR 0015 §2, 0016). Without rules it always can.
///
/// A glob without `!` lists ignored and hidden files in the folders the search enters, and
/// a `-t` type lists hidden ones. The index holds them: what its walk lists, and by name
/// what it leaves out of the folders it enters (ADR 0016). What it can't answer:
///
/// * A walk that reads only some ignore files (`--no-ignore-dot`, `--ignore-file`...).
/// * A glob that names a folder the search wouldn't enter makes the walk enter it, and the
///   index holds nothing inside.
/// * A `!` glob can leave out folders the index recorded errors for, or that the caller may
///   not list. Those are reported per folder; rather than work out which reports remain,
///   such searches are left to the scanner. Both are rare.
pub fn unanswerable(
    index: &Index,
    target: Target<'_>,
    rules: &Rules,
    viewer: &dyn Viewer,
) -> Result<Option<&'static str>> {
    if rules.is_empty() {
        return Ok(None);
    }
    if !rules.ignore_files().is_standard_for(&target.filter) {
        return Ok(Some(
            "the index records walks that read all ignore files or none",
        ));
    }
    let bit = membership::bit(&target.filter);
    if rules.widening() == Widening::All {
        let dirs = index.dirs()?;
        let everywhere = target.dir.as_os_str().is_empty();
        for dir in dirs.iter() {
            if dir.root != 0 || dir.flags & bit != 0 {
                continue;
            }
            let Ok(rest) = dir.path.strip_prefix(target.dir) else {
                continue;
            };
            if (everywhere || !rest.as_os_str().is_empty())
                && rules.glob(&target.root.join(rest), true) == Verdict::Include
            {
                return Ok(Some("a glob names a folder the search wouldn't enter"));
            }
        }
    }
    if rules.prunes() {
        let hidden = unlisted(index, target, bit, viewer)?;
        if !hidden.is_empty() || !stored_errors(index, target, bit, &hidden).is_empty() {
            return Ok(Some(
                "a ! glob may leave out a folder the index has an error for",
            ));
        }
    }
    Ok(None)
}

/// The walk bit of `filter` with hidden files included.
fn hidden_bit(filter: Filter) -> u8 {
    membership::bit(&Filter {
        include_hidden: true,
        ..filter
    })
}

/// Which files of an index a search lists: those its filter's walk lists, or with glob and
/// type rules, those the walk with the rules lists (ADR 0015 §2). Only for searches the
/// index can answer ([`unanswerable`]).
struct Listing<'a> {
    target: Target<'a>,
    rules: &'a Rules,
    bit: u8,
    hidden_bit: u8,
    /// Whether the index records the walk with hidden files on (`hidden_bit`). If not, the
    /// walker decides for the files that matter ([`Listing::not_ignored`]).
    hidden_known: bool,
    /// What the walk with hidden files on lists in a folder (relative to the search root),
    /// for the folders the walker was asked about.
    walked: HashMap<PathBuf, HashSet<std::ffi::OsString>>,
    /// The membership bits of each folder (relative to the index's root), when the rules can
    /// list files the filter alone doesn't.
    dirs: HashMap<PathBuf, u8>,
    /// The bits of the index's root.
    root_bits: u8,
    /// Whether a folder (relative to the search root) is left out by a `!` glob, itself or
    /// through a folder above it.
    pruned: HashMap<PathBuf, bool>,
}

impl<'a> Listing<'a> {
    fn new(index: &Index, target: Target<'a>, rules: &'a Rules) -> Result<Self, IndexError> {
        let dirs = if rules.widening() == Widening::None {
            HashMap::new()
        } else {
            index
                .dirs()?
                .iter()
                .filter(|d| d.root == 0)
                .map(|d| (d.path.clone(), d.flags))
                .collect()
        };
        let root_bits = membership::covered(&index.coverage());
        let hidden_bit = hidden_bit(target.filter);
        Ok(Self {
            target,
            rules,
            bit: membership::bit(&target.filter),
            hidden_bit,
            hidden_known: root_bits & hidden_bit != 0,
            walked: HashMap::new(),
            dirs,
            root_bits,
            pruned: HashMap::new(),
        })
    }

    /// Whether the search lists file `d` of `segment`.
    fn lists(&mut self, segment: &Segment, d: u32) -> bool {
        let flags = segment.flags(d);
        let listed = flags & self.bit != 0;
        if self.rules.is_empty() {
            return listed;
        }
        let (_, relative) = segment.path(d);
        let Ok(rest) = relative.strip_prefix(self.target.dir) else {
            return false;
        };
        if let Some(parent) = rest.parent()
            && self.pruned(parent)
        {
            return false;
        }
        // A file the walk lists is in a folder it enters; for the others, look it up. Every
        // file in such a folder is in the index, by name at least (ADR 0016).
        let entered = listed || self.entered(relative.parent());
        let known = Known {
            listed,
            exists: entered,
        };
        let rules = self.rules;
        let spelled = self.target.root.join(rest);
        rules.lists(&spelled, known, &mut || self.not_ignored(flags, rest))
    }

    /// Whether ignore files leave out a file (`rest`, relative to the search root, with
    /// `flags`) in a folder the search enters, for a `-t` type. From its walk bits if the
    /// index records the walk with hidden files on; else the walker lists the file's folder,
    /// one level deep, with hidden files on, as the scanner would (the subtree rule of
    /// ADR 0012 makes that walk agree with one from the root).
    fn not_ignored(&mut self, flags: u8, rest: &Path) -> bool {
        if self.hidden_known {
            return flags & self.hidden_bit != 0;
        }
        let folder = rest.parent().unwrap_or_else(|| Path::new(""));
        let Some(name) = rest.file_name() else {
            return false;
        };
        if !self.walked.contains_key(folder) {
            let listed = walk_folder(&self.target.disk_root.join(folder), self.target.filter);
            self.walked.insert(folder.to_path_buf(), listed);
        }
        self.walked
            .get(folder)
            .is_some_and(|names| names.contains(name))
    }

    /// Whether the search's walk enters folder `dir` (relative to the index's root).
    fn entered(&self, dir: Option<&Path>) -> bool {
        let bits = match dir {
            None => self.root_bits,
            Some(d) if d.as_os_str().is_empty() => self.root_bits,
            Some(d) => self.dirs.get(d).copied().unwrap_or(0),
        };
        bits & self.bit != 0
    }

    /// Whether folder `rest` (relative to the search root) is left out by a `!` glob.
    fn pruned(&mut self, rest: &Path) -> bool {
        if rest.as_os_str().is_empty() || !self.rules.prunes() {
            return false;
        }
        if let Some(&known) = self.pruned.get(rest) {
            return known;
        }
        let above = rest.parent().is_some_and(|p| self.pruned(p));
        let pruned =
            above || self.rules.glob(&self.target.root.join(rest), true) == Verdict::Exclude;
        self.pruned.insert(rest.to_path_buf(), pruned);
        pruned
    }
}

/// The names of the files the walk with `filter`, hidden files on, lists directly in
/// `folder`.
fn walk_folder(folder: &Path, filter: Filter) -> HashSet<std::ffi::OsString> {
    let scope = mtseek_core::Scope {
        roots: vec![folder.to_path_buf()],
        filter: Filter {
            include_hidden: true,
            ..filter
        },
    };
    let names = Mutex::new(HashSet::new());
    mtseek_walk::list(
        &scope,
        Options { threads: 1 },
        Some(1),
        &|entry| {
            if entry.depth == 1
                && !entry.is_dir
                && let Some(name) = entry.path.file_name()
            {
                names
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .insert(name.to_os_string());
            }
        },
        &|_| {},
    );
    names.into_inner().unwrap_or_else(PoisonError::into_inner)
}

/// The errors the index recorded that walk `bit` from `target` reports, outside folders the
/// caller may not list, spelled onto the root as typed.
fn stored_errors(index: &Index, target: Target<'_>, bit: u8, hidden: &[PathBuf]) -> Vec<Error> {
    let mut out = Vec::new();
    for stored in index.errors() {
        if stored.flags & bit == 0 {
            continue;
        }
        let path = match stored.root {
            Some(_) => {
                if hidden.iter().any(|d| stored.path.starts_with(d)) {
                    continue;
                }
                match stored.path.strip_prefix(target.dir) {
                    Ok(rest) => target.root.join(rest),
                    Err(_) => continue,
                }
            }
            None => stored.path.clone(),
        };
        out.push(Error::Io {
            path,
            source: std::io::Error::other(stored.message.clone()),
        });
    }
    out
}

/// What one layer of the index shows the caller: which of its descriptors let it read a
/// file, and the ranges of files under folders it may not list.
struct LayerView {
    readable: Vec<bool>,
    pruned: Vec<(u32, u32)>,
}

impl LayerView {
    fn new(segment: &Segment, hidden: &[PathBuf], viewer: &dyn Viewer) -> Self {
        Self {
            readable: segment
                .descriptors()
                .iter()
                .map(|sd| viewer.may_read(sd))
                .collect(),
            pruned: hidden.iter().map(|d| range_under(segment, d)).collect(),
        }
    }

    fn readable(&self, segment: &Segment, d: u32) -> bool {
        self.readable[segment.descriptor_id(d) as usize]
    }

    fn pruned(&self, d: u32) -> bool {
        self.pruned.iter().any(|&(a, b)| d >= a && d < b)
    }

    /// Whether the caller may see file `d`.
    fn visible(&self, segment: &Segment, d: u32) -> bool {
        self.readable(segment, d) && !self.pruned(d)
    }

    /// The files in `range` that the caller's own `rg` would fail to open: listed by the
    /// search, under no pruned folder, not replaced by the delta (`skip`).
    fn unreadable(
        &self,
        segment: &Segment,
        skip: &[u32],
        range: (u32, u32),
        listed: &mut dyn FnMut(u32) -> bool,
    ) -> Vec<u32> {
        if !self.readable.contains(&false) {
            return Vec::new();
        }
        let mut skip = skip.iter().peekable();
        let mut out = Vec::new();
        for d in range.0..range.1 {
            while skip.next_if(|&&s| s < d).is_some() {}
            if skip.peek() == Some(&&d) || self.readable(segment, d) || self.pruned(d) {
                continue;
            }
            if listed(d) {
                out.push(d);
            }
        }
        out
    }
}

/// The files of `segment` in `range` that may match `plan`. A file the index doesn't store
/// (too big, secret, unread) has no trigrams: it is a candidate whatever the plan says, and
/// is read from disk. So, in `binary` mode, is a cut file, whose trigrams stop at its first
/// NUL while binary mode reads on.
fn layer_candidates(
    plan: &Plan,
    segment: &Segment,
    (lo, hi): (u32, u32),
    binary: bool,
) -> Result<Vec<u32>, IndexError> {
    let found: Vec<u32> = match candidates(plan, segment)? {
        Some(list) => list.into_iter().filter(|&d| d >= lo && d < hi).collect(),
        None => (lo..hi).collect(),
    };
    let from_disk: Vec<u32> = (lo..hi)
        .filter(|&d| {
            segment.location(d) == Location::External
                || (binary && segment.flags(d) & membership::TRUNCATED != 0)
        })
        .collect();
    Ok(if from_disk.is_empty() {
        found
    } else {
        union(&found, &from_disk)
    })
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
    bytes: &'a AtomicU64,
    nanos: &'a AtomicU64,
}

impl Worker<'_> {
    fn run(
        &self,
        matcher: &mtseek_walk::RegexMatcher,
        output: Output,
        work: &[Work<'_>],
        next: &AtomicUsize,
        blocks: &AtomicU64,
    ) -> Result<(), IndexError> {
        let mut totals = Summary::default();
        let result = self.search(matcher, output, work, next, blocks, &mut totals);
        self.bytes
            .fetch_add(totals.bytes_searched, Ordering::Relaxed);
        let nanos = u64::try_from(totals.search_time.as_nanos()).unwrap_or(u64::MAX);
        self.nanos.fetch_add(nanos, Ordering::Relaxed);
        result
    }

    fn search(
        &self,
        matcher: &mtseek_walk::RegexMatcher,
        output: Output,
        work: &[Work<'_>],
        next: &AtomicUsize,
        blocks: &AtomicU64,
        totals: &mut Summary,
    ) -> Result<(), IndexError> {
        let mut readers = Vec::with_capacity(self.layers.len());
        for (segment, _) in self.layers {
            readers.push(segment.block_reader()?);
        }
        let mut searcher = FileSearcher::new(self.target.filter, output);
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
                let Ok(rest) = relative.strip_prefix(self.target.dir) else {
                    continue; // outside the folder: excluded by the range already
                };
                let path = self.target.root.join(rest);
                let on_disk = || self.target.disk_root.join(rest);
                let found = match segment.location(doc) {
                    _ if from_disk(doc, segment) => searcher.search_path(matcher, &on_disk()),
                    Location::Block { offset, len, .. } => {
                        let bytes = &raw[offset as usize..(offset + len) as usize];
                        searcher.search_bytes(matcher, bytes)
                    }
                    Location::Empty => searcher.search_bytes(matcher, &[]),
                    Location::External => searcher.search_path(matcher, &on_disk()),
                };
                let found = match found {
                    Ok(Some(found)) => found,
                    Ok(None) => {
                        totals.add_file(&searcher.last());
                        continue;
                    }
                    Err(source) => {
                        self.errors.fetch_add(1, Ordering::Relaxed);
                        (self.on_error)(&Error::Io { path, source });
                        continue;
                    }
                };
                totals.add_file(&found.stats);
                self.matched.fetch_add(1, Ordering::Relaxed);
                let hit = FileHit {
                    path: &path,
                    lines: found.lines,
                    binary_offset: found.binary_offset,
                    stats: found.stats,
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

/// The topmost folders under `target.dir` (itself included) that the walk enters and the
/// caller may not list, relative to the index's root. `rg` reports each once and goes no
/// further into it.
fn unlisted(
    index: &Index,
    target: Target<'_>,
    bit: u8,
    viewer: &dyn Viewer,
) -> Result<Vec<PathBuf>, IndexError> {
    // Entries share their descriptor with every other entry that has the same one, so a
    // decision is looked up by the descriptor's address: a few dozen checks at most.
    let mut decided: HashMap<*const u8, bool> = HashMap::new();
    let mut hidden: Vec<PathBuf> = Vec::new();
    let dirs = index.dirs()?;
    let everywhere = target.dir.as_os_str().is_empty();
    // Parents come before their children, so a pruned folder is found before what it holds.
    for dir in dirs.iter() {
        let inside = dir.root == 0 && (everywhere || dir.path.starts_with(target.dir));
        let entered = dir.flags & bit != 0 || dir.path == target.dir;
        if !inside || !entered {
            continue;
        }
        let allowed = *decided
            .entry(dir.sd.as_ptr())
            .or_insert_with(|| viewer.may_list(&dir.sd));
        if !allowed && !hidden.iter().any(|h| dir.path.starts_with(h)) {
            hidden.push(dir.path.clone());
        }
    }
    Ok(hidden)
}

/// Why the service must leave this search to the caller's own process (ADR 0014 §6), if it
/// must: the search reaches a file the index never read that the caller may read (an
/// encrypted file, a cloud placeholder), or the caller may not read an ignore file the walk
/// would read, so its own `rg` would list other files.
pub fn decline_reason(
    index: &Index,
    target: Target<'_>,
    rules: &Rules,
    viewer: &dyn Viewer,
) -> Result<Option<&'static str>> {
    if let Some(why) = unanswerable(index, target, rules, viewer)? {
        return Ok(Some(why));
    }
    let bit = membership::bit(&target.filter);
    let hidden = unlisted(index, target, bit, viewer)?;
    let mut listing = Listing::new(index, target, rules)?;
    for (segment, skip) in index.layers() {
        let (lo, hi) = range_under(segment, target.dir);
        let unstored = (lo..hi)
            .filter(|d| skip.binary_search(d).is_err())
            .filter(|&d| segment.flags(d) & membership::UNSTORED != 0 && listing.lists(segment, d))
            .any(|d| {
                let (_, path) = segment.path(d);
                viewer.may_read(segment.descriptor(d))
                    && !hidden.iter().any(|h| path.starts_with(h))
            });
        if unstored {
            return Ok(Some(
                "the search reaches an encrypted or cloud-only file the index doesn't hold",
            ));
        }
    }
    for control in index.controls()? {
        let Some(name) = control.path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let is_rules = [".gitignore", ".ignore", ".rgignore", "exclude"]
            .iter()
            .any(|r| r.eq_ignore_ascii_case(name));
        if !is_rules || control.sd.is_empty() {
            continue;
        }
        // The folder whose walk the file affects: its repository for `.git\info\exclude`.
        let parent = control.path.parent().unwrap_or_else(|| Path::new(""));
        let folder = if name.eq_ignore_ascii_case("exclude") {
            parent.parent().and_then(Path::parent).unwrap_or(parent)
        } else {
            parent
        };
        let above = matches!(
            control.path.components().next(),
            Some(std::path::Component::ParentDir)
        );
        let applies = above || target.dir.starts_with(folder) || folder.starts_with(target.dir);
        if applies && !viewer.may_read(&control.sd) {
            return Ok(Some(
                "the caller may not read an ignore file the search would read",
            ));
        }
    }
    Ok(None)
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
