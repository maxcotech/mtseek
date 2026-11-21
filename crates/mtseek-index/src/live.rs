//! Keeping the content index current (ADR 0011): an immutable base segment, a delta segment
//! with every file added or re-read since, and catch-up from the NTFS change journal.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};

use mtseek_core::Scope;
use mtseek_walk::Options;

use crate::build::{Found, coverage, list_level, new_build_id, read_file, walk_tree};
use crate::{
    Checkpoint, Content, Doc, Entry, IndexError, Location, Meta, Segment, SegmentInfo, StoredError,
    WriteStats, membership, ntfs,
};

/// A delta holding more files than this (or 1/8 of the base, if more) is folded into a
/// rebuilt base instead of being rewritten.
const MAX_DELTA_FILES: u64 = 2_000;
/// Irrelevant journal records are left for later searches to skip, until this many pile up.
const MAX_SKIPPED_RECORDS: usize = 20_000;

/// A base segment and, if anything changed since it was built, its delta.
#[derive(Debug)]
pub struct Index {
    base: Segment,
    delta: Option<Segment>,
    base_path: PathBuf,
}

/// What [`Index::catch_up`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CatchUp {
    /// Nothing relevant changed. `records` journal records were read.
    Current {
        /// Journal records read (all irrelevant).
        records: usize,
    },
    /// The delta was rewritten.
    Updated {
        /// Journal records read.
        records: usize,
        /// Directories re-listed.
        relisted: usize,
        /// Subtrees walked again (new directories, changed ignore files).
        walked: usize,
        /// Files read.
        read: usize,
    },
    /// The index can't be caught up and must be rebuilt.
    NeedsRebuild(&'static str),
}

impl Index {
    /// Opens the index at `base_path`, with its delta if there is a matching one.
    /// `None` if there is no usable base built for `key`.
    pub fn open(base_path: &Path, key: &str) -> Option<Self> {
        let base = Segment::open(base_path).ok().filter(|s| s.key() == key)?;
        let delta = Segment::open(&delta_path(base_path)).ok().filter(|d| {
            d.key() == key
                && d.meta().base_id == base.meta().build_id
                && d.tombstones().last().is_none_or(|&t| t < base.len())
        });
        Some(Self {
            base,
            delta,
            base_path: base_path.to_path_buf(),
        })
    }

    /// Builds a fresh base for `scope` at `base_path` and removes any delta.
    pub fn build(
        scope: &Scope,
        options: Options,
        base_path: &Path,
    ) -> Result<(Self, WriteStats), IndexError> {
        let _ = std::fs::remove_file(delta_path(base_path));
        let stats = crate::build(scope, options, base_path)?;
        let base = Segment::open(base_path)?;
        Ok((
            Self {
                base,
                delta: None,
                base_path: base_path.to_path_buf(),
            },
            stats,
        ))
    }

    /// The base segment.
    pub fn base(&self) -> &Segment {
        &self.base
    }

    /// The delta segment, if any.
    pub fn delta(&self) -> Option<&Segment> {
        self.delta.as_ref()
    }

    /// The segments to search, each with the files of it to skip (increasing ids).
    pub fn layers(&self) -> Vec<(&Segment, &[u32])> {
        let tombstones = self.delta.as_ref().map_or(&[][..], Segment::tombstones);
        let mut layers = vec![(&self.base, tombstones)];
        if let Some(delta) = &self.delta {
            layers.push((delta, &[][..]));
        }
        layers
    }

    /// The number of files the index answers for.
    pub fn len(&self) -> u64 {
        self.layers()
            .iter()
            .map(|(s, skip)| u64::from(s.len()) - skip.len() as u64)
            .sum()
    }

    /// Whether the index holds no files.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Files and directories that couldn't be read, as of the last build or catch-up.
    pub fn errors(&self) -> &[StoredError] {
        self.newest().errors()
    }

    /// Build id, base id and journal checkpoints of the newest layer.
    pub fn meta(&self) -> &Meta {
        self.newest().meta()
    }

    /// The bytes on disk of base and delta.
    pub fn file_bytes(&self) -> u64 {
        self.layers().iter().map(|(s, _)| s.file_bytes()).sum()
    }

    /// The membership bits of folder `dir` (relative to the root): which filters' walks enter
    /// it. `None` if the walk that built the index doesn't enter it (ADR 0012).
    pub fn entered(&self, dir: &Path) -> Result<Option<u8>, IndexError> {
        if dir.as_os_str().is_empty() {
            return Ok(Some(membership::covered(&self.coverage())));
        }
        Ok(self
            .newest()
            .dirs()?
            .into_iter()
            .find(|e| e.root == 0 && e.path == dir)
            .map(|e| e.flags))
    }

    /// The coverage filter the index was built with, read from its key.
    pub fn coverage(&self) -> mtseek_core::Filter {
        let key = self.base.key();
        let mut filter = mtseek_core::Filter::unrestricted(0);
        filter.respect_ignore_files = !key.contains("ignore files=false");
        filter.include_hidden = key.contains("hidden=true");
        filter
    }

    fn newest(&self) -> &Segment {
        self.delta.as_ref().unwrap_or(&self.base)
    }

    /// Applies every change the journal recorded since the index was last current
    /// (ADR 0011). On [`CatchUp::NeedsRebuild`] the index is unchanged and the caller
    /// should [`Index::build`] it.
    pub fn catch_up(&mut self, scope: &Scope, options: Options) -> Result<CatchUp, IndexError> {
        if scope.filter.follow_links {
            return Ok(CatchUp::NeedsRebuild("the scope follows links"));
        }
        let scope = &Scope {
            roots: scope.roots.clone(),
            filter: coverage(scope.filter),
        };
        let meta = self.meta().clone();
        if meta.checkpoints.is_empty() {
            return Ok(CatchUp::NeedsRebuild("no change journal"));
        }
        let mut records: Vec<(u64, u64)> = Vec::new();
        let mut checkpoints = Vec::new();
        for from in &meta.checkpoints {
            match ntfs::read_journal(from, &mut |frn, parent| records.push((frn, parent))) {
                Ok(next) => checkpoints.push(next),
                Err(ntfs::JournalError::Lost) => {
                    return Ok(CatchUp::NeedsRebuild(
                        "the change journal no longer covers the index",
                    ));
                }
                Err(ntfs::JournalError::Unavailable) => {
                    return Ok(CatchUp::NeedsRebuild("the change journal can't be read"));
                }
            }
        }
        if records.is_empty() {
            return Ok(CatchUp::Current { records: 0 });
        }
        // Most records are about the rest of the drive. Check cheaply before loading paths.
        if records.len() <= MAX_SKIPPED_RECORDS && !self.any_tracked(&records)? {
            return Ok(CatchUp::Current {
                records: records.len(),
            });
        }
        let mut state = State::load(self)?;
        let work = state.classify(&records);
        if work.is_empty() && records.len() <= MAX_SKIPPED_RECORDS {
            return Ok(CatchUp::Current {
                records: records.len(),
            });
        }
        let mut counts = Stats::default();
        if let Err(reason) = state.apply(scope, options, work, &mut counts) {
            return Ok(CatchUp::NeedsRebuild(reason));
        }
        let limit = MAX_DELTA_FILES.max(u64::from(self.base.len()) / 8);
        if state.delta_files() > limit {
            return Ok(CatchUp::NeedsRebuild("many files changed"));
        }
        self.write_delta(scope, options, state, checkpoints)?;
        Ok(CatchUp::Updated {
            records: records.len(),
            relisted: counts.relisted,
            walked: counts.walked,
            read: counts.read,
        })
    }

    /// Whether any record names a tracked file, directory or control file, as itself or as
    /// the parent. Only FRNs are compared, no paths decoded.
    fn any_tracked(&self, records: &[(u64, u64)]) -> Result<bool, IndexError> {
        let newest = self.newest();
        let mut tracked: HashSet<u64> = HashSet::new();
        for (segment, _) in self.layers() {
            tracked.extend((0..segment.len()).map(|id| segment.frn(id)));
        }
        tracked.extend(newest.dirs()?.into_iter().map(|e| e.frn));
        tracked.extend(newest.controls()?.into_iter().map(|e| e.frn));
        tracked.remove(&0);
        Ok(records
            .iter()
            .any(|(frn, parent)| tracked.contains(frn) || tracked.contains(parent)))
    }

    fn write_delta(
        &mut self,
        scope: &Scope,
        options: Options,
        state: State,
        checkpoints: Vec<Checkpoint>,
    ) -> Result<(), IndexError> {
        let mut kept_base = vec![false; self.base.len() as usize];
        let mut docs = Vec::new();
        let mut reader = self.delta.as_ref().map(Segment::block_reader).transpose()?;
        for ((root, SortPath { path, .. }), live) in state.docs {
            let content = match live.source {
                Source::Base(id) => {
                    kept_base[id as usize] = true;
                    continue;
                }
                Source::New(content) => content,
                Source::Delta(id) => {
                    let (Some(delta), Some(reader)) = (&self.delta, reader.as_mut()) else {
                        continue;
                    };
                    match delta.location(id) {
                        Location::Empty => Content::Stored(Vec::new()),
                        Location::External => Content::External,
                        Location::Block { block, offset, len } => {
                            let raw = reader.read(block)?;
                            Content::Stored(raw[offset as usize..(offset + len) as usize].to_vec())
                        }
                    }
                }
            };
            docs.push(Doc {
                root,
                path,
                frn: live.frn,
                flags: live.flags,
                content,
            });
        }
        drop(reader);
        let tombstones: Vec<u32> = (0u32..)
            .zip(&kept_base)
            .filter(|&(_, &kept)| !kept)
            .map(|(id, _)| id)
            .collect();
        let dirs: Vec<Entry> = state
            .dirs
            .into_iter()
            .map(|((root, SortPath { path, .. }), (frn, flags))| Entry {
                frn,
                root,
                path,
                flags,
            })
            .collect();
        let controls: Vec<Entry> = state
            .controls
            .into_iter()
            .map(|((root, SortPath { path, .. }), frn)| Entry {
                frn,
                root,
                path,
                flags: 0,
            })
            .collect();
        let meta = Meta {
            build_id: new_build_id(),
            base_id: self.base.meta().build_id,
            checkpoints,
        };
        let key = self.base.key().to_owned();
        let info = SegmentInfo {
            key: &key,
            roots: u32::try_from(scope.roots.len()).unwrap_or(u32::MAX),
            errors: &state.errors,
            dirs: &dirs,
            controls: &controls,
            tombstones: &tombstones,
            meta: &meta,
        };
        let path = delta_path(&self.base_path);
        // Close the old delta first: Windows can't replace a file that is open.
        self.delta = None;
        crate::write_segment(&path, &info, &mut docs, options.thread_count())?;
        self.delta = Some(Segment::open(&path)?);
        Ok(())
    }
}

/// Where the delta of the base at `base_path` lives.
pub fn delta_path(base_path: &Path) -> PathBuf {
    base_path.with_extension("delta")
}

#[derive(Debug, Default)]
struct Stats {
    relisted: usize,
    walked: usize,
    read: usize,
}

/// Where a live file's bytes come from.
#[derive(Debug)]
enum Source {
    Base(u32),
    Delta(u32),
    New(Content),
}

#[derive(Debug)]
struct Live {
    frn: u64,
    /// Membership bits ([`membership::FILTERS`] only).
    flags: u8,
    source: Source,
}

/// A path relative to its root, ordered by its bytes with separators lowest. That keeps a
/// directory's descendants right after it, and for normalised paths it is exactly `Path`'s
/// order, but compares with `memcmp` (15,000 inserts with `PathBuf` keys took 38 ms).
#[derive(Debug, Clone, Default)]
struct SortPath {
    path: PathBuf,
    sort: Box<[u8]>,
}

impl SortPath {
    fn new(path: PathBuf) -> Self {
        let sort = path
            .as_os_str()
            .as_encoded_bytes()
            .iter()
            .map(|&b| if b == b'\\' || b == b'/' { 0 } else { b })
            .collect();
        Self { path, sort }
    }
}

impl PartialEq for SortPath {
    fn eq(&self, other: &Self) -> bool {
        self.sort == other.sort
    }
}

impl Eq for SortPath {}

impl PartialOrd for SortPath {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for SortPath {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.sort.cmp(&other.sort)
    }
}

impl std::hash::Hash for SortPath {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.sort.hash(state);
    }
}

impl std::ops::Deref for SortPath {
    type Target = Path;
    fn deref(&self) -> &Path {
        &self.path
    }
}

impl AsRef<Path> for SortPath {
    fn as_ref(&self) -> &Path {
        &self.path
    }
}

type Key = (u32, SortPath);

fn make_key(root: u32, path: PathBuf) -> Key {
    (root, SortPath::new(path))
}

/// The index as an editable set of files, directories (FRN and membership bits), control
/// files and errors.
struct State {
    docs: BTreeMap<Key, Live>,
    dirs: BTreeMap<Key, (u64, u8)>,
    controls: BTreeMap<Key, u64>,
    errors: Vec<StoredError>,
}

/// What the journal says needs looking at.
#[derive(Debug, Default)]
struct Work {
    /// Directories to re-list.
    dirty: BTreeSet<Key>,
    /// Directories whose whole subtree must be walked again.
    rewalk: BTreeSet<Key>,
    /// FRNs of indexed files that changed in any way.
    changed: HashSet<u64>,
}

impl Work {
    fn is_empty(&self) -> bool {
        self.dirty.is_empty() && self.rewalk.is_empty()
    }
}

impl State {
    fn load(index: &Index) -> Result<Self, IndexError> {
        // Segments list files in path order, so collecting is a linear bulk build.
        let mut docs = Vec::new();
        for (layer, (segment, skip)) in index.layers().into_iter().enumerate() {
            let mut skip = skip.iter().peekable();
            for id in 0..segment.len() {
                if skip.next_if_eq(&&id).is_some() {
                    continue;
                }
                let (root, path) = segment.path(id);
                let source = if layer == 0 {
                    Source::Base(id)
                } else {
                    Source::Delta(id)
                };
                let root = u32::try_from(root).unwrap_or(u32::MAX);
                docs.push((
                    make_key(root, path),
                    Live {
                        frn: segment.frn(id),
                        flags: segment.flags(id) & membership::FILTERS,
                        source,
                    },
                ));
            }
        }
        // Two sorted runs (base, delta): a stable sort merges them cheaply.
        docs.sort_by(|a, b| a.0.cmp(&b.0));
        let docs: BTreeMap<Key, Live> = docs.into_iter().collect();
        let newest = index.newest();
        Ok(Self {
            docs,
            dirs: newest
                .dirs()?
                .into_iter()
                .map(|e| (make_key(e.root, e.path), (e.frn, e.flags)))
                .collect(),
            controls: newest
                .controls()?
                .into_iter()
                .map(|e| (make_key(e.root, e.path), e.frn))
                .collect(),
            errors: newest.errors().to_vec(),
        })
    }

    fn delta_files(&self) -> u64 {
        self.docs
            .values()
            .filter(|l| !matches!(l.source, Source::Base(_)))
            .count() as u64
    }

    /// Maps journal records `(frn, parent frn)` to the directories that need looking at.
    fn classify(&self, records: &[(u64, u64)]) -> Work {
        fn by_frn<'a>(items: impl Iterator<Item = (&'a Key, u64)>) -> HashMap<u64, Vec<&'a Key>> {
            let mut map: HashMap<u64, Vec<&Key>> = HashMap::new();
            for (key, frn) in items {
                if frn != 0 {
                    map.entry(frn).or_default().push(key);
                }
            }
            map
        }
        let docs = by_frn(self.docs.iter().map(|(k, l)| (k, l.frn)));
        let dirs = by_frn(self.dirs.iter().map(|(k, &(f, _))| (k, f)));
        let controls = by_frn(self.controls.iter().map(|(k, &f)| (k, f)));
        let mut work = Work::default();
        for &(frn, parent) in records {
            for key in dirs.get(&parent).into_iter().flatten() {
                work.dirty.insert((*key).clone());
            }
            for key in docs.get(&frn).into_iter().flatten() {
                work.changed.insert(frn);
                work.dirty.insert(parent_of(key));
            }
            for key in dirs.get(&frn).into_iter().flatten() {
                if !key.1.as_os_str().is_empty() {
                    work.dirty.insert(parent_of(key));
                }
            }
            for key in controls.get(&frn).into_iter().flatten() {
                work.rewalk.insert(parent_of(key));
            }
        }
        work
    }

    /// Re-walks and re-lists what `work` names. `Err` if the index must be rebuilt.
    fn apply(
        &mut self,
        scope: &Scope,
        options: Options,
        work: Work,
        stats: &mut Stats,
    ) -> Result<(), &'static str> {
        let Work {
            dirty,
            rewalk,
            changed,
        } = work;
        let mut walked: Vec<Key> = Vec::new();
        for key in &rewalk {
            if walked.iter().any(|w| is_under(key, w)) {
                continue;
            }
            // A folder the index doesn't hold is settled by its parent's re-listing.
            if let Some(&(_, within)) = self.dirs.get(key) {
                self.rewalk(scope, options, key, within, stats);
                walked.push(key.clone());
            }
        }
        // Parents before children, so a child already handled by its parent is skipped.
        let mut queue: BTreeSet<(usize, Key)> = dirty
            .into_iter()
            .map(|k| (k.1.components().count(), k))
            .collect();
        let mut done: HashSet<Key> = HashSet::new();
        while let Some((_, key)) = queue.pop_first() {
            if walked.iter().any(|w| is_under(&key, w))
                || !self.dirs.contains_key(&key)
                || !done.insert(key.clone())
            {
                continue;
            }
            match self.relist(scope, options, &key, &changed, stats) {
                Relisted::Done => {}
                Relisted::Walked => walked.push(key),
                Relisted::Gone => {
                    if key.1.as_os_str().is_empty() {
                        return Err("a root can no longer be listed");
                    }
                    let parent = parent_of(&key);
                    done.remove(&parent);
                    queue.insert((parent.1.components().count(), parent));
                }
            }
        }
        Ok(())
    }

    /// Re-lists directory `key` and brings its direct entries up to date.
    fn relist(
        &mut self,
        scope: &Scope,
        options: Options,
        key: &Key,
        changed: &HashSet<u64>,
        stats: &mut Stats,
    ) -> Relisted {
        let (root, dir) = key;
        let full = scope.roots[*root as usize].join(dir);
        let Some(&(frn_before, within)) = self.dirs.get(key) else {
            return Relisted::Done;
        };
        let Ok(listing) = ntfs::list_dir(&full) else {
            self.remove_under(key);
            return Relisted::Gone;
        };
        stats.relisted += 1;
        // A different directory under the old name, or changed ignore rules: walk it all.
        let controls_now: BTreeMap<Key, u64> = listing
            .entries
            .iter()
            .filter(|e| ntfs::is_control(&e.name))
            .map(|e| (make_key(*root, dir.join(&e.name)), e.frn))
            .collect();
        let controls_before: BTreeMap<Key, u64> = children(&self.controls, key)
            .map(|(k, &f)| (k.clone(), f))
            .collect();
        if frn_before != listing.frn || controls_now != controls_before {
            self.rewalk(scope, options, key, within, stats);
            return Relisted::Walked;
        }
        let frns: HashMap<&str, u64> = listing
            .entries
            .iter()
            .map(|e| (e.name.as_str(), e.frn))
            .collect();
        let frn_of = |path: &Path| {
            path.file_name()
                .and_then(|n| n.to_str())
                .and_then(|n| frns.get(n).copied())
                .unwrap_or(0)
        };
        let level = list_level(scope, *root as usize, dir, within);

        self.refresh_files(scope, key, &level.files, &frn_of, changed, stats);
        let listed_files: HashSet<&PathBuf> = level.files.iter().map(|(p, _)| p).collect();
        self.errors.retain(|e| {
            !(e.root == Some(*root)
                && (e.path == dir.path
                    || (e.path.parent() == Some(&**dir)
                        && !self.dirs.contains_key(&key_of(*root, &e.path))
                        && !listed_files.contains(&e.path))))
        });
        self.errors.extend(level.errors);

        // Subdirectories: walk the new, the replaced and those whose membership changed (it
        // changes for everything under them); drop the vanished.
        let listed_dirs: HashSet<&PathBuf> = level.dirs.iter().map(|(p, _)| p).collect();
        let vanished: Vec<Key> = children(&self.dirs, key)
            .filter(|(k, _)| !listed_dirs.contains(&k.1.path))
            .map(|(k, _)| k.clone())
            .collect();
        for k in vanished {
            self.remove_under(&k);
        }
        for (path, flags) in &level.dirs {
            let sub = key_of(*root, path);
            let frn = frn_of(path);
            if frn == 0 || self.dirs.get(&sub) != Some(&(frn, *flags)) {
                self.rewalk(scope, options, &sub, *flags, stats);
            }
        }
        Relisted::Done
    }

    /// Brings the files directly in directory `key` up to date with `listed`: keeps the
    /// unchanged, reads the rest, drops the vanished.
    fn refresh_files(
        &mut self,
        scope: &Scope,
        key: &Key,
        listed: &[(PathBuf, u8)],
        frn_of: &dyn Fn(&Path) -> u64,
        changed: &HashSet<u64>,
        stats: &mut Stats,
    ) {
        let root = &key.0;
        let wanted: HashSet<&PathBuf> = listed.iter().map(|(p, _)| p).collect();
        let gone: Vec<Key> = children(&self.docs, key)
            .filter(|(k, _)| !wanted.contains(&k.1.path))
            .map(|(k, _)| k.clone())
            .collect();
        for k in gone {
            self.docs.remove(&k);
        }
        // Errors about these files are recomputed below.
        self.errors
            .retain(|e| !(e.root == Some(*root) && wanted.contains(&e.path)));
        for (path, flags) in listed {
            let flags = *flags;
            let frn = frn_of(path);
            let doc_key = key_of(*root, path);
            let unchanged = self.docs.get(&doc_key).is_some_and(|l| {
                frn != 0 && l.frn == frn && l.flags == flags && !changed.contains(&frn)
            });
            if unchanged {
                continue;
            }
            stats.read += 1;
            let full = scope.roots[*root as usize].join(path);
            match read_file(&full, scope.filter.include_binary) {
                Ok((content, frn)) => {
                    self.docs.insert(
                        doc_key,
                        Live {
                            frn,
                            flags,
                            source: Source::New(content),
                        },
                    );
                }
                Err(e) => {
                    self.docs.remove(&doc_key);
                    self.errors.push(StoredError {
                        root: Some(*root),
                        path: path.clone(),
                        message: e.to_string(),
                        flags,
                    });
                }
            }
        }
    }

    /// Drops everything under directory `key` and walks it again. `within` is the membership
    /// of `key` itself.
    fn rewalk(
        &mut self,
        scope: &Scope,
        options: Options,
        key: &Key,
        within: u8,
        stats: &mut Stats,
    ) {
        self.remove_under(key);
        let (root, dir) = key;
        if !scope.roots[*root as usize].join(dir).is_dir() {
            return; // gone: its parent's re-listing (it is dirty too) settles the rest
        }
        stats.walked += 1;
        let found: Found = walk_tree(scope, *root as usize, dir, within, options);
        stats.read += found.docs.len();
        for doc in found.docs {
            self.docs.insert(
                make_key(doc.root, doc.path),
                Live {
                    frn: doc.frn,
                    flags: doc.flags,
                    source: Source::New(doc.content),
                },
            );
        }
        for e in found.dirs {
            self.dirs.insert(make_key(e.root, e.path), (e.frn, e.flags));
        }
        for e in found.controls {
            self.controls.insert(make_key(e.root, e.path), e.frn);
        }
        self.errors.extend(found.errors);
    }

    /// Removes directory `key`, and everything under it, from every table.
    fn remove_under(&mut self, key: &Key) {
        fn drop_range<V>(map: &mut BTreeMap<Key, V>, key: &Key) {
            let doomed: Vec<Key> = map
                .range(key.clone()..)
                .take_while(|(k, _)| is_under(k, key))
                .map(|(k, _)| k.clone())
                .collect();
            for k in doomed {
                map.remove(&k);
            }
        }
        drop_range(&mut self.docs, key);
        drop_range(&mut self.dirs, key);
        drop_range(&mut self.controls, key);
        self.errors
            .retain(|e| !(e.root == Some(key.0) && e.path.starts_with(&*key.1)));
    }
}

enum Relisted {
    Done,
    Walked,
    Gone,
}

/// Whether `key` is `dir` or below it.
fn is_under(key: &Key, dir: &Key) -> bool {
    key.0 == dir.0 && key.1.starts_with(&*dir.1)
}

fn parent_of(k: &Key) -> Key {
    make_key(k.0, k.1.parent().map(Path::to_path_buf).unwrap_or_default())
}

fn key_of(root: u32, path: &Path) -> Key {
    make_key(root, path.to_path_buf())
}

/// The entries of `map` directly inside directory `dir` (not `dir` itself).
fn children<'a, V>(
    map: &'a BTreeMap<Key, V>,
    dir: &'a Key,
) -> impl Iterator<Item = (&'a Key, &'a V)> {
    map.range(dir.clone()..)
        .take_while(move |(k, _)| is_under(k, dir))
        .filter(move |(k, _)| k.1.parent() == Some(&*dir.1))
}
