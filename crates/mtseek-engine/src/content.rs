//! Content search through coverage roots (ADR 0010, 0011, 0012).
//!
//! The cache directory holds a list of **coverage roots**, each a folder indexed with one
//! filter, and one index per root. `--index` adds the folders of a search to the list. From
//! then on every content search inside a covered folder uses its index, as long as the
//! search's filter lists no more than the index's filter does: a search of a subfolder, of
//! the same folder spelled differently (case, 8.3 short names), or with narrower filters.
//! Each search first applies what the NTFS change journal recorded since the index was last
//! current, so results are never stale. Roots the index can't answer exactly are scanned.

use std::collections::HashSet;
use std::fmt::Write as _;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, Instant};

use mtseek_core::{Error, Filter, Result, Scope};
use mtseek_index::{CatchUp, Index, Persist, membership};
use mtseek_query::Viewer;
use mtseek_walk::{FileHit, Options, Rules};

use crate::{Engine, IndexUse, Open, Outcome, Report, SearchRequest, io_error, ms};

/// The coverage list, one root per line: `<ignore files><hidden>\t<canonical path>`, with
/// `1`/`0` for each flag.
pub const REGISTRY: &str = "content-coverage.txt";

/// Who a search runs for and what it may do.
pub(crate) struct View<'a> {
    /// What the caller may see.
    pub(crate) viewer: &'a dyn Viewer,
    /// Where each root is on this machine, when the caller spelled them in a namespace this
    /// process doesn't share (the service).
    pub(crate) disk_roots: Option<&'a [PathBuf]>,
    /// Answer only from indexes, exactly, or decline before reporting anything (the
    /// service, ADR 0014 §6).
    pub(crate) only_indexed: bool,
}

/// Searches each root through the index that covers it, where one can answer exactly, and
/// scans the rest: no covering index (and `--index` not given), a filter the index doesn't
/// cover, globs or types that list files it doesn't hold, `--no-index`, a search that follows
/// links, a file, or a path through a junction.
pub(crate) fn search(
    engine: &mut Engine,
    request: &SearchRequest,
    rules: &Rules,
    report: &dyn Report,
    view: &View<'_>,
) -> Result<Outcome> {
    let options = Options {
        threads: request.threads,
    };
    let filter = request.filter;
    let mut outcome = Outcome::default();
    let rest = indexed(engine, request, rules, options, report, view, &mut outcome)?;
    if !rest.is_empty() {
        if view.only_indexed {
            return Err(Error::Declined("a root no index answers".into()));
        }
        let scope = Scope {
            roots: rest,
            filter,
        };
        let on_hit = |hit: &FileHit<'_>| report.hit(hit);
        let on_error = |err: &Error| report.error(err);
        let scanned = mtseek_walk::scan(
            &scope,
            rules,
            &request.query,
            request.output,
            options,
            &on_hit,
            &on_error,
        )?;
        outcome.summary.add(scanned);
    }
    if !engine.resident {
        // The CLI wrote every change before searching; this only closes the indexes.
        engine.indexes.clear();
    }
    Ok(outcome)
}

/// The indexed part of [`search`]. Returns the roots left to scan.
fn indexed(
    engine: &mut Engine,
    request: &SearchRequest,
    rules: &Rules,
    options: Options,
    report: &dyn Report,
    view: &View<'_>,
    outcome: &mut Outcome,
) -> Result<Vec<PathBuf>> {
    let explicit = request.index.explicit();
    if !uses_index(request, report, view)? {
        return Ok(request.roots.clone());
    }
    let Some(cache) = engine.cache.clone() else {
        if explicit {
            return Err(io_error(
                Path::new("<cache>"),
                io::Error::new(
                    io::ErrorKind::NotFound,
                    "no cache directory: set MTSEEK_CACHE_DIR or LOCALAPPDATA",
                ),
            ));
        }
        return Ok(request.roots.clone());
    };

    let mut run = Run {
        engine,
        index_use: request.index,
        options,
        cache: &cache,
        registry: Registry::load(&cache),
        current: HashSet::new(),
        rebuilt: HashSet::new(),
        report,
        stats: &mut outcome.stats,
    };
    let mut rest = Vec::new();
    // First every root's index, opened and caught up, so that a search the service must
    // decline is declined before anything is reported.
    let routes = run.routes(request, rules, view)?;
    for Route { typed, disk, found } in routes {
        let Some((root, dir)) = found else {
            rest.push(typed.clone());
            continue;
        };
        let Some(index) = run.index(&root)? else {
            rest.push(typed.clone());
            continue;
        };
        let target = mtseek_query::Target {
            root: typed,
            disk_root: disk,
            dir: &dir,
            filter: request.filter,
        };
        let unanswerable = match mtseek_query::unanswerable(index, target, rules, view.viewer) {
            Ok(why) => why,
            Err(err) => return Err(run.discard(&root, err)),
        };
        if let Some(why) = unanswerable {
            if explicit {
                report.note(&format!("{}: {why}; scanning", typed.display()));
            }
            rest.push(typed.clone());
            continue;
        }
        let on_hit = |hit: &FileHit<'_>| report.hit(hit);
        let on_error = |err: &Error| report.error(err);
        let searched = Instant::now();
        let result = mtseek_query::search(
            index,
            target,
            rules,
            &request.query,
            request.output,
            options,
            view.viewer,
            &on_hit,
            &on_error,
        );
        let (found, search_stats) = match result {
            Ok(done) => done,
            Err(err @ Error::InvalidPattern(_)) => return Err(err),
            Err(err) => return Err(run.discard(&root, err)),
        };
        run.stats
            .push(search_line(typed, &search_stats, &found, searched));
        outcome.summary.add(found);
    }
    run.registry.save(&cache, report);
    if !rest.is_empty() && !run.stats.is_empty() {
        run.stats
            .push(format!("{} root(s) scanned instead", rest.len()));
    }
    Ok(rest)
}

/// Whether a search may use content indexes at all. For the service, [`Error::Declined`]
/// if it may not.
fn uses_index(request: &SearchRequest, report: &dyn Report, view: &View<'_>) -> Result<bool> {
    let why = if request.index == IndexUse::Off {
        None
    } else if request.filter.follow_links {
        Some("searches that follow links (-L) don't use the content index; scanning")
    } else if !request.select.ignore_files.is_standard_for(&request.filter) {
        // No index records a walk that reads only some ignore files.
        Some(
            "--no-ignore-vcs, --ignore-file and the like don't use the content index yet; scanning",
        )
    } else {
        return Ok(true);
    };
    if view.only_indexed {
        return Err(Error::Declined(
            "the search doesn't use the content index".to_owned(),
        ));
    }
    if let Some(why) = why
        && request.index.explicit()
    {
        report.note(why);
    }
    Ok(false)
}

/// The `--stats` line for a search of root `typed` through an index.
fn search_line(
    typed: &Path,
    stats: &mtseek_query::SearchStats,
    found: &mtseek_walk::Summary,
    started: Instant,
) -> String {
    let mut plan = stats.plan.to_string();
    if plan.len() > 300 {
        let cut = (0..=300).rev().find(|&i| plan.is_char_boundary(i));
        plan.truncate(cut.unwrap_or(0));
        plan.push_str("...");
    }
    format!(
        "{}: {} candidates, {} blocks read, {} matched in {:.1} ms; plan {plan}",
        typed.display(),
        stats.candidates,
        stats.blocks,
        found.files_matched,
        ms(started.elapsed()),
    )
}

/// Catches up every open index (see [`Engine::refresh`]).
pub(crate) fn refresh(engine: &mut Engine) -> usize {
    let mut changed = 0;
    let mut closed = Vec::new();
    for (root, open) in &mut engine.indexes {
        let scope = Scope {
            roots: vec![root.clone()],
            filter: open.index.coverage(),
        };
        match open.index.catch_up(&scope, Options::default()) {
            Ok(CatchUp::Current { .. }) => {}
            Ok(CatchUp::Updated { .. }) => {
                open.changed = Some(Instant::now());
                changed += 1;
            }
            Ok(CatchUp::NeedsRebuild(_)) | Err(_) => closed.push(root.clone()),
        }
    }
    for root in closed {
        // Unsaved changes are lost with it; the rebuild re-reads everything.
        engine.indexes.remove(&root);
    }
    changed
}

/// Writes unsaved deltas older than `quiet` (see [`Engine::save`]).
pub(crate) fn save(engine: &mut Engine, quiet: Duration) -> Vec<String> {
    let mut lines = Vec::new();
    for (root, open) in &mut engine.indexes {
        if !open.index.unsaved() || open.changed.is_some_and(|t| t.elapsed() < quiet) {
            continue;
        }
        let started = Instant::now();
        match open.index.save() {
            Ok(_) => lines.push(format!(
                "wrote the delta of {} in {:.1} ms",
                root.display(),
                ms(started.elapsed())
            )),
            Err(err) => lines.push(format!(
                "could not write the delta of {}: {err}",
                root.display()
            )),
        }
    }
    lines
}

/// How one root of a search is answered.
struct Route<'a> {
    /// The root as typed.
    typed: &'a PathBuf,
    /// Where it is on this machine.
    disk: &'a PathBuf,
    /// The coverage root whose index answers it, and its folder there; `None` to scan.
    found: Option<(PathBuf, PathBuf)>,
}

/// One search's view of the indexes: the coverage list, which indexes it caught up or
/// rebuilt, and its `--stats` lines.
struct Run<'a> {
    engine: &'a mut Engine,
    index_use: IndexUse,
    options: Options,
    cache: &'a Path,
    registry: Registry,
    /// Coverage roots whose index this search caught up (or built).
    current: HashSet<PathBuf>,
    rebuilt: HashSet<PathBuf>,
    report: &'a dyn Report,
    stats: &'a mut Vec<String>,
}

impl Run<'_> {
    /// The coverage root that answers a search of `typed` with `filter`, and the folder of
    /// it that `typed` names. Registers `typed` first if `--index` asks for it. `None` if
    /// `typed` is to be scanned.
    fn resolve(&mut self, typed: &Path, filter: Filter) -> Option<(PathBuf, PathBuf)> {
        let explicit = self.index_use.explicit();
        // A missing root or a file: the scanner reports it the way rg does.
        let canonical = canonical(typed).filter(|c| c.is_dir())?;
        // The index's walk read the ignore files above its root along the real path. Through
        // a junction or link the search would read others, so only plain spellings qualify.
        let Ok(absolute) = std::path::absolute(typed) else {
            return None;
        };
        if !same_path(&absolute, &canonical) {
            if explicit {
                self.report.note(&format!(
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
                    self.report.note(&format!(
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
                    self.report.note(&format!(
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

    /// How each root of `request` is answered, with every index it needs opened and caught
    /// up. For the service (`view.only_indexed`), [`Error::Declined`] unless every root is
    /// answered by an index exactly.
    fn routes<'r>(
        &mut self,
        request: &'r SearchRequest,
        rules: &Rules,
        view: &View<'r>,
    ) -> Result<Vec<Route<'r>>> {
        let decline = |why: &str| Err(Error::Declined(why.to_owned()));
        let mut routes = Vec::with_capacity(request.roots.len());
        for (i, typed) in request.roots.iter().enumerate() {
            let disk = view.disk_roots.map_or(typed, |d| &d[i]);
            let found = match self.resolve(disk, request.filter) {
                Some((root, dir)) => self
                    .route(&root, &dir, request.filter)?
                    .then_some((root, dir)),
                None => None,
            };
            if view.only_indexed {
                let Some((root, dir)) = &found else {
                    return decline("a root isn't inside a folder the service indexes");
                };
                let target = mtseek_query::Target {
                    root: typed,
                    disk_root: disk,
                    dir,
                    filter: request.filter,
                };
                if let Some(index) = self.index(root)?
                    && let Some(why) =
                        mtseek_query::decline_reason(index, target, rules, view.viewer)?
                {
                    return decline(why);
                }
            }
            routes.push(Route { typed, disk, found });
        }
        Ok(routes)
    }

    /// Whether the index of coverage root `root` answers a search of its folder `dir` with
    /// `filter`: it can be opened, and its walk enters `dir` with that filter. Otherwise the
    /// walker decides what is in `dir` (an ignored or hidden folder, searched by name).
    fn route(&mut self, root: &Path, dir: &Path, filter: Filter) -> Result<bool> {
        let Some(index) = self.index(root)? else {
            return Ok(false);
        };
        let bit = membership::bit(&filter);
        match index.entered(dir) {
            Ok(flags) => Ok(flags.is_some_and(|flags| flags & bit != 0)),
            Err(err) => Err(self.discard(root, err.into())),
        }
    }

    /// The index of coverage root `root`, caught up (once per search), or built. `None` if
    /// it can't be used: missing, or not kept current, without `--index`.
    fn index(&mut self, root: &Path) -> Result<Option<&Index>> {
        let explicit = self.index_use.explicit();
        let Some(entry) = self.registry.get(root).cloned() else {
            self.forget(root);
            return Ok(None);
        };
        let scope = Scope {
            roots: vec![root.to_path_buf()],
            filter: entry.filter,
        };
        // An index kept open from an earlier search (the daemon) is used if another process
        // hasn't changed its coverage since.
        if let Some(open) = self.engine.indexes.get(root)
            && open.index.coverage() != mtseek_index::coverage(entry.filter)
        {
            self.forget(root);
        }
        let reindex =
            self.index_use == IndexUse::Rebuild && self.rebuilt.insert(root.to_path_buf());
        if reindex {
            self.forget(root);
        }
        if self.current.contains(root) {
            return Ok(self.engine.indexes.get(root).map(|o| &o.index));
        }
        let path = index_path(self.cache, root);
        let started = Instant::now();
        let opened = match self.engine.indexes.remove(root) {
            Some(open) => Some(open),
            None if reindex => None,
            None => {
                let key =
                    mtseek_index::scope_key(&scope).map_err(|source| io_error(root, source))?;
                Index::open(&path, &key).map(|index| Open {
                    index,
                    changed: None,
                })
            }
        };
        let open = match opened {
            // New, or registered earlier but gone or unreadable: build it.
            None => self.build(&scope, &path, started)?,
            Some(open) if open.index.meta().checkpoints.is_empty() && !explicit => {
                return Ok(None);
            }
            Some(mut open) => {
                let opened = started.elapsed();
                open.index.set_persist(self.persist());
                match open.index.catch_up(&scope, self.options)? {
                    CatchUp::NeedsRebuild(reason) => {
                        self.report
                            .note(&format!("rebuilding the content index: {reason}"));
                        drop(open);
                        self.build(&scope, &path, started)?
                    }
                    done => {
                        if matches!(done, CatchUp::Updated { .. }) {
                            open.changed = Some(Instant::now());
                        }
                        self.stats.push(format!(
                            "index of {} files ({:.1} MiB) for {} opened in {:.1} ms, \
                             caught up in {:.1} ms: {}",
                            open.index.len(),
                            mib(open.index.file_bytes()),
                            root.display(),
                            ms(opened),
                            ms(started.elapsed().saturating_sub(opened)),
                            describe(&done),
                        ));
                        open
                    }
                }
            }
        };
        self.stats.push(format!("index file {}", path.display()));
        self.current.insert(root.to_path_buf());
        self.engine.indexes.insert(root.to_path_buf(), open);
        Ok(self.engine.indexes.get(root).map(|o| &o.index))
    }

    fn persist(&self) -> Persist {
        if self.engine.resident {
            Persist::Later
        } else {
            Persist::Now
        }
    }

    fn build(&mut self, scope: &Scope, path: &Path, started: Instant) -> Result<Open> {
        std::fs::create_dir_all(self.cache).map_err(|source| io_error(self.cache, source))?;
        let root = scope
            .roots
            .first()
            .map_or_else(|| Path::new(""), PathBuf::as_path);
        self.report.note(&format!(
            "building the content index for {} (only the first time)",
            root.display()
        ));
        let (mut index, built) = Index::build(scope, self.options, path)?;
        index.set_persist(self.persist());
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
        Ok(Open {
            index,
            changed: None,
        })
    }

    /// Closes the index of `root`, if open.
    fn forget(&mut self, root: &Path) {
        self.engine.indexes.remove(root);
        self.current.remove(root);
    }

    /// Drops a broken index, so the next search builds a fresh one, and passes on `err`.
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

    /// Writes the list if it changed. Failing only costs the next search its index.
    fn save(&self, cache: &Path, report: &dyn Report) {
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
            report.note(&format!(
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
        struct Quiet;
        impl Report for Quiet {
            fn hit(&self, _: &FileHit<'_>) -> std::ops::ControlFlow<()> {
                std::ops::ControlFlow::Continue(())
            }
            fn error(&self, _: &Error) {}
            fn note(&self, _: &str) {}
        }
        let dir = tempfile::tempdir().unwrap();
        let mut registry = Registry::load(dir.path());
        registry.set(Path::new(r"C:\Dev"), Filter::unrestricted(1));
        registry.set(Path::new(r"D:\Notes"), Filter::unrestricted(0));
        registry.save(dir.path(), &Quiet);
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
