//! Content search through the content index (ADR 0010, 0011, 0012).
//!
//! The cache directory holds a list of **coverage roots**, each a folder indexed with one
//! filter, and one index per root. `--index` adds the folders of a search to the list. From
//! then on every content search inside a covered folder uses its index, as long as the
//! search's filter lists no more than the index's filter does: a search of a subfolder, of
//! the same folder spelled differently (case, 8.3 short names), or with narrower filters.
//! Each run first applies what the NTFS change journal recorded since the last run, so
//! results are never stale. Roots the index can't answer exactly are scanned. `--no-index`
//! scans everything; `--reindex` rebuilds.

use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::io::{self, Write};
use std::ops::ControlFlow;
use std::path::{Component, Path, PathBuf};
use std::time::Instant;

use mtseek_core::{Error, Filter, Query, Result, Scope};
use mtseek_index::{CatchUp, Index, membership};
use mtseek_walk::{FileHit, Mode, Options, Summary};

use crate::Args;
use crate::names::{cache_dir, hint, ms};

/// The coverage list, one root per line: `<ignore files><hidden>\t<canonical path>`, with
/// `1`/`0` for each flag.
const REGISTRY: &str = "content-coverage.txt";

/// What [`search`] did: the totals for the roots it answered from an index, and the roots
/// left for the caller to scan.
pub(crate) struct Outcome {
    pub(crate) summary: Summary,
    pub(crate) rest: Vec<PathBuf>,
}

/// Searches each root of `scope` through the index that covers it, where one can answer
/// exactly (ADR 0012). The other roots are returned for scanning: no covering index (and
/// `--index` not given), a filter the index doesn't cover, `--no-index`, a search that follows
/// links, a file, or a path through a junction.
pub(crate) fn search(
    args: &Args,
    scope: &Scope,
    query: &Query,
    mode: Mode,
    options: Options,
    on_hit: &(dyn Fn(&FileHit<'_>) -> ControlFlow<()> + Sync),
    on_error: &(dyn Fn(&Error) + Sync),
) -> Result<Outcome> {
    let explicit = args.index || args.reindex;
    let everything = || Outcome {
        summary: Summary::default(),
        rest: scope.roots.clone(),
    };
    if args.no_index {
        return Ok(everything());
    }
    if scope.filter.follow_links {
        if explicit {
            hint("searches that follow links (-L) don't use the content index; scanning");
        }
        return Ok(everything());
    }
    let Some(cache) = cache_dir() else {
        if explicit {
            return Err(io_error(
                Path::new("<cache>"),
                io::Error::new(
                    io::ErrorKind::NotFound,
                    "no cache directory: set MTSEEK_CACHE_DIR or LOCALAPPDATA",
                ),
            ));
        }
        return Ok(everything());
    };

    let mut run = Run {
        args,
        options,
        cache: &cache,
        registry: Registry::load(&cache),
        open: HashMap::new(),
        rebuilt: HashSet::new(),
        stats: Vec::new(),
    };
    let mut summary = Summary::default();
    let mut rest = Vec::new();
    for typed in &scope.roots {
        let Some((root, dir)) = run.resolve(typed, scope.filter) else {
            rest.push(typed.clone());
            continue;
        };
        let Some(index) = run.index(&root)? else {
            rest.push(typed.clone());
            continue;
        };
        let bit = membership::bit(&scope.filter);
        let entered = match index.entered(&dir) {
            Ok(flags) => flags,
            Err(err) => return Err(run.discard(&root, err.into())),
        };
        if entered.is_none_or(|flags| flags & bit == 0) {
            // A folder the index's walk skips (an ignored or hidden folder, searched by
            // name): the walker decides what is in it.
            rest.push(typed.clone());
            continue;
        }
        let target = mtseek_query::Target {
            root: typed,
            dir: &dir,
            filter: scope.filter,
        };
        let searched = Instant::now();
        let result = mtseek_query::search(index, target, query, mode, options, on_hit, on_error);
        let (found, search_stats) = match result {
            Ok(done) => done,
            Err(err @ Error::InvalidPattern(_)) => return Err(err),
            Err(err) => return Err(run.discard(&root, err)),
        };
        let mut plan = search_stats.plan.to_string();
        if plan.len() > 300 {
            let cut = (0..=300).rev().find(|&i| plan.is_char_boundary(i));
            plan.truncate(cut.unwrap_or(0));
            plan.push_str("...");
        }
        run.stats.push(format!(
            "{}: {} candidates, {} blocks read, {} matched in {:.1} ms; plan {plan}",
            typed.display(),
            search_stats.candidates,
            search_stats.blocks,
            found.files_matched,
            ms(searched.elapsed()),
        ));
        summary.files_searched += found.files_searched;
        summary.files_matched += found.files_matched;
        summary.errors += found.errors;
    }
    run.registry.save(&cache);
    if args.stats {
        let mut err = io::stderr().lock();
        for line in &run.stats {
            let _ = writeln!(err, "{line}");
        }
        if !rest.is_empty() && !run.stats.is_empty() {
            let _ = writeln!(err, "{} root(s) scanned instead", rest.len());
        }
    }
    Ok(Outcome { summary, rest })
}

/// One run's indexes: the coverage list, the indexes opened (and caught up) so far, and the
/// `--stats` lines.
struct Run<'a> {
    args: &'a Args,
    options: Options,
    cache: &'a Path,
    registry: Registry,
    open: HashMap<PathBuf, Index>,
    rebuilt: HashSet<PathBuf>,
    stats: Vec<String>,
}

impl Run<'_> {
    /// The coverage root that answers a search of `typed` with `filter`, and the folder of
    /// it that `typed` names. Registers `typed` first if `--index` asks for it. `None` if
    /// `typed` is to be scanned.
    fn resolve(&mut self, typed: &Path, filter: Filter) -> Option<(PathBuf, PathBuf)> {
        let explicit = self.args.index || self.args.reindex;
        // A missing root or a file: the scanner reports it the way rg does.
        let canonical = canonical(typed).filter(|c| c.is_dir())?;
        // The index's walk read the ignore files above its root along the real path. Through
        // a junction or link the search would read others, so only plain spellings qualify.
        let Ok(absolute) = std::path::absolute(typed) else {
            return None;
        };
        if !same_path(&absolute, &canonical) {
            if explicit {
                hint(&format!(
                    "{}: the path goes through a link or junction; scanning",
                    typed.display()
                ));
            }
            return None;
        }
        let wanted = mtseek_index::coverage(filter);
        let root = match self.registry.covering(&canonical).cloned() {
            Some(entry) if covers(entry.filter, filter) => entry.root,
            Some(entry) if explicit && entry.root == canonical => {
                // The same folder with filters that list more: widen its index.
                let widened = union(entry.filter, wanted);
                self.registry.set(&canonical, widened);
                self.forget(&canonical);
                self.rebuilt.remove(&canonical);
                canonical.clone()
            }
            Some(entry) => {
                if explicit {
                    hint(&format!(
                        "{} is inside {}, whose index lists fewer files than these filters \
                         do; scanning. Run --index on {} with these filters to widen it.",
                        typed.display(),
                        entry.root.display(),
                        entry.root.display()
                    ));
                }
                return None;
            }
            None if explicit => {
                let inside = self.registry.inside(&canonical);
                if let Some(wider) = inside.iter().find(|e| !covers(wanted, e.filter)) {
                    hint(&format!(
                        "{} holds {}, indexed with filters that list more; scanning. \
                         Use the same filters to index {} instead.",
                        typed.display(),
                        wider.root.display(),
                        typed.display()
                    ));
                    return None;
                }
                for nested in inside {
                    self.forget(&nested.root);
                    remove_index(&index_path(self.cache, &nested.root));
                    self.registry.remove(&nested.root);
                }
                self.registry.set(&canonical, wanted);
                canonical.clone()
            }
            None => return None,
        };
        let dir = canonical
            .strip_prefix(&root)
            .map(Path::to_path_buf)
            .unwrap_or_default();
        Some((root, dir))
    }

    /// The index of coverage root `root`, opened and caught up (once per run), or built.
    /// `None` if it can't be used: missing, or not kept current, without `--index`.
    fn index(&mut self, root: &Path) -> Result<Option<&Index>> {
        if self.open.contains_key(root) {
            return Ok(self.open.get(root));
        }
        let explicit = self.args.index || self.args.reindex;
        let Some(entry) = self.registry.get(root).cloned() else {
            return Ok(None);
        };
        let scope = Scope {
            roots: vec![root.to_path_buf()],
            filter: entry.filter,
        };
        let path = index_path(self.cache, root);
        let key = mtseek_index::scope_key(&scope).map_err(|source| io_error(root, source))?;
        let started = Instant::now();
        let reindex = self.args.reindex && self.rebuilt.insert(root.to_path_buf());
        let existing = if reindex {
            None
        } else {
            Index::open(&path, &key)
        };
        let index = match existing {
            // New, or registered earlier but gone or unreadable: build it.
            None => self.build(&scope, &path, started)?,
            Some(index) if index.meta().checkpoints.is_empty() && !explicit => return Ok(None),
            Some(mut index) => {
                let opened = started.elapsed();
                match index.catch_up(&scope, self.options)? {
                    CatchUp::NeedsRebuild(reason) => {
                        hint(&format!("rebuilding the content index: {reason}"));
                        drop(index);
                        self.build(&scope, &path, started)?
                    }
                    done => {
                        self.stats.push(format!(
                            "index of {} files ({:.1} MiB) for {} opened in {:.1} ms, \
                             caught up in {:.1} ms: {}",
                            index.len(),
                            mib(index.file_bytes()),
                            root.display(),
                            ms(opened),
                            ms(started.elapsed().saturating_sub(opened)),
                            describe(&done),
                        ));
                        index
                    }
                }
            }
        };
        self.stats.push(format!("index file {}", path.display()));
        self.open.insert(root.to_path_buf(), index);
        Ok(self.open.get(root))
    }

    fn build(&mut self, scope: &Scope, path: &Path, started: Instant) -> Result<Index> {
        std::fs::create_dir_all(self.cache).map_err(|source| io_error(self.cache, source))?;
        let root = scope
            .roots
            .first()
            .map_or_else(|| Path::new(""), PathBuf::as_path);
        hint(&format!(
            "building the content index for {} (only the first time)",
            root.display()
        ));
        let (index, built) = Index::build(scope, self.options, path)?;
        self.stats.push(format!(
            "index of {} files built in {:.0} ms: {:.1} MB of text stored as {:.1} MB, \
             {} trigrams, {} postings, {:.1} MiB on disk, {} unreadable, {}",
            built.files,
            ms(started.elapsed()),
            megabytes(built.stored_bytes),
            megabytes(built.compressed_bytes),
            built.trigrams,
            built.postings,
            mib(built.file_bytes),
            index.errors().len(),
            if index.meta().checkpoints.is_empty() {
                "no change journal (it will be rebuilt for each --index search)"
            } else {
                "kept current from the change journal"
            },
        ));
        Ok(index)
    }

    /// Closes the index of `root`, if open.
    fn forget(&mut self, root: &Path) {
        self.open.remove(root);
    }

    /// Drops a broken index, so the next run builds a fresh one, and passes on `err`.
    fn discard(&mut self, root: &Path, err: Error) -> Error {
        self.forget(root);
        remove_index(&index_path(self.cache, root));
        err
    }
}

/// A coverage root and its filter.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Covered {
    root: PathBuf,
    filter: Filter,
}

/// The coverage list ([`REGISTRY`]).
#[derive(Debug, Default)]
struct Registry {
    entries: Vec<Covered>,
    changed: bool,
}

impl Registry {
    fn load(cache: &Path) -> Self {
        let text = std::fs::read_to_string(cache.join(REGISTRY)).unwrap_or_default();
        let entries = text
            .lines()
            .filter_map(|line| {
                let (flags, root) = line.split_once('\t')?;
                let mut filter = Filter::unrestricted(0);
                match flags.as_bytes() {
                    [i, h] => {
                        filter.respect_ignore_files = *i == b'1';
                        filter.include_hidden = *h == b'1';
                    }
                    _ => return None,
                }
                Some(Covered {
                    root: PathBuf::from(root),
                    filter,
                })
            })
            .collect();
        Self {
            entries,
            changed: false,
        }
    }

    /// Writes the list if it changed. Failing only costs the next run its index.
    fn save(&self, cache: &Path) {
        if !self.changed {
            return;
        }
        let mut text = String::new();
        for e in &self.entries {
            let _ = writeln!(
                text,
                "{}{}\t{}",
                u8::from(e.filter.respect_ignore_files),
                u8::from(e.filter.include_hidden),
                e.root.display()
            );
        }
        let path = cache.join(REGISTRY);
        let tmp = path.with_extension(format!("tmp{}", std::process::id()));
        let written = std::fs::create_dir_all(cache)
            .and_then(|()| std::fs::write(&tmp, text))
            .and_then(|()| std::fs::rename(&tmp, &path));
        if let Err(e) = written {
            let _ = std::fs::remove_file(&tmp);
            hint(&format!(
                "could not save the content index list {}: {e}",
                path.display()
            ));
        }
    }

    fn get(&self, root: &Path) -> Option<&Covered> {
        self.entries.iter().find(|e| e.root == root)
    }

    /// The coverage root `path` is in (or is).
    fn covering(&self, path: &Path) -> Option<&Covered> {
        self.entries.iter().find(|e| path.starts_with(&e.root))
    }

    /// The coverage roots strictly inside `path`.
    fn inside(&self, path: &Path) -> Vec<Covered> {
        self.entries
            .iter()
            .filter(|e| e.root != path && e.root.starts_with(path))
            .cloned()
            .collect()
    }

    fn set(&mut self, root: &Path, filter: Filter) {
        self.remove(root);
        self.entries.push(Covered {
            root: root.to_path_buf(),
            filter,
        });
        self.changed = true;
    }

    fn remove(&mut self, root: &Path) {
        let before = self.entries.len();
        self.entries.retain(|e| e.root != root);
        self.changed |= self.entries.len() != before;
    }
}

/// Whether an index built with `coverage` answers a search with `filter` exactly.
fn covers(coverage: Filter, filter: Filter) -> bool {
    membership::covered(&coverage) & membership::bit(&filter) != 0
}

/// The narrowest coverage filter that covers both.
fn union(a: Filter, b: Filter) -> Filter {
    Filter {
        respect_ignore_files: a.respect_ignore_files && b.respect_ignore_files,
        include_hidden: a.include_hidden || b.include_hidden,
        ..mtseek_index::coverage(a)
    }
}

/// The real path of `path` as NTFS spells it (long names, stored case, links resolved),
/// without the `\\?\` prefix for ordinary drive paths.
fn canonical(path: &Path) -> Option<PathBuf> {
    let full = std::fs::canonicalize(path).ok()?;
    let text = full.to_str()?;
    Some(match text.strip_prefix(r"\\?\") {
        Some(rest) if rest.as_bytes().get(1) == Some(&b':') => PathBuf::from(rest),
        _ => full,
    })
}

/// Whether `typed` (absolute) names `canonical` without going through a link: the same
/// components, equal but for ASCII case or 8.3 short names (which contain `~`).
fn same_path(typed: &Path, canonical: &Path) -> bool {
    let a: Vec<Component<'_>> = typed.components().collect();
    let b: Vec<Component<'_>> = canonical.components().collect();
    a.len() == b.len()
        && a.iter().zip(&b).all(|(x, y)| {
            let (x, y) = (
                x.as_os_str().to_string_lossy(),
                y.as_os_str().to_string_lossy(),
            );
            x.eq_ignore_ascii_case(&y) || x.contains('~')
        })
}

/// Where the index of coverage root `root` lives.
fn index_path(cache: &Path, root: &Path) -> PathBuf {
    let hash = root
        .to_string_lossy()
        .bytes()
        .fold(0xcbf2_9ce4_8422_2325_u64, |h, b| {
            (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
        });
    cache.join(format!("content-{hash:016x}.idx"))
}

fn remove_index(path: &Path) {
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_file(mtseek_index::delta_path(path));
}

fn describe(done: &CatchUp) -> String {
    match done {
        CatchUp::Current { records } => format!("current ({records} journal records)"),
        CatchUp::Updated {
            records,
            relisted,
            walked,
            read,
        } => format!(
            "{records} journal records, {relisted} directories re-listed, \
             {walked} walked, {read} files read"
        ),
        CatchUp::NeedsRebuild(reason) => format!("needs a rebuild: {reason}"),
    }
}

#[allow(clippy::cast_precision_loss)] // a size for people to read
fn mib(bytes: u64) -> f64 {
    bytes as f64 / f64::from(1 << 20)
}

#[allow(clippy::cast_precision_loss)] // a size for people to read
fn megabytes(bytes: u64) -> f64 {
    bytes as f64 / 1e6
}

fn io_error(path: &Path, source: io::Error) -> Error {
    Error::Io {
        path: path.to_path_buf(),
        source,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spellings_of_the_same_path_match_and_links_do_not() {
        let canonical = Path::new(r"C:\Users\runneradmin\AppData\Local\Temp\x");
        assert!(same_path(
            Path::new(r"c:\users\RUNNER~1\appdata\local\temp\X"),
            canonical
        ));
        assert!(!same_path(Path::new(r"D:\link\x"), canonical));
        assert!(!same_path(
            Path::new(r"C:\Users\other\AppData\Local\Temp\x"),
            canonical
        ));
    }

    #[test]
    fn coverage_and_union() {
        let default = Filter::unrestricted(0);
        let no_ignore = Filter::unrestricted(1);
        let mut hidden = Filter::unrestricted(0);
        hidden.include_hidden = true;
        assert!(covers(no_ignore, default));
        assert!(!covers(default, no_ignore));
        assert!(!covers(no_ignore, hidden));
        let both = union(no_ignore, hidden);
        assert!(covers(both, hidden) && covers(both, no_ignore));
        assert_eq!(both, Filter::unrestricted(2));
        let mut binary = Filter::unrestricted(3);
        assert!(
            covers(both, binary),
            "binary mode reads cut files from disk"
        );
        binary.follow_links = true;
        assert_eq!(mtseek_index::coverage(binary), both);
    }

    #[test]
    fn the_registry_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let mut registry = Registry::load(dir.path());
        registry.set(Path::new(r"C:\Dev"), Filter::unrestricted(1));
        registry.set(Path::new(r"D:\Notes"), Filter::unrestricted(0));
        registry.save(dir.path());
        let loaded = Registry::load(dir.path());
        assert_eq!(loaded.entries, registry.entries);
        assert_eq!(
            loaded.covering(Path::new(r"C:\Dev\api")).map(|e| &e.root),
            Some(&PathBuf::from(r"C:\Dev"))
        );
        assert!(loaded.covering(Path::new(r"C:\Devices")).is_none());
        assert_eq!(loaded.inside(Path::new(r"C:\")).len(), 1);
    }
}
