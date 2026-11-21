//! File-name search (`--names`).
//!
//! Name indexes are cached on disk (in the cache directory) with the change-journal position
//! they reflect. A search loads one and applies only what changed since, which needs no
//! admin rights. Without a usable cache, it builds the index: from the NTFS master file
//! table when elevated (the whole drive), otherwise by walking the root. A resident engine
//! (the daemon) keeps them in memory and catches them up in place.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use mtseek_core::Result;
use mtseek_names::{FileId, NameQuery, Origin, Snapshot};

use crate::{Engine, NamesOutcome, NamesRequest, Report, io_error, ms};

/// A name index in memory, and what it took to get it.
#[derive(Debug)]
pub(crate) struct Loaded {
    snapshot: Snapshot,
    /// How it was obtained, for `--stats`.
    how: String,
    took: Duration,
    /// Where to save it, if it changed since it was last saved and has a journal position.
    save_to: Option<PathBuf>,
    /// When it last changed.
    changed: Instant,
}

/// The name indexes an engine holds: whole-drive (MFT) ones by drive letter, shared by
/// every root on the drive, and walked ones by root.
#[derive(Debug, Default)]
pub(crate) struct Names {
    drives: HashMap<char, Loaded>,
    walked: HashMap<PathBuf, Loaded>,
}

impl Names {
    pub(crate) fn status(&self) -> Vec<(PathBuf, usize)> {
        let mut out: Vec<(PathBuf, usize)> = self
            .drives
            .values()
            .chain(self.walked.values())
            .map(|l| {
                (
                    l.snapshot.index.root_path().to_path_buf(),
                    l.snapshot.index.len(),
                )
            })
            .collect();
        out.sort();
        out
    }
}

/// Which index answers a root: a drive's, or a walked one.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum Source {
    Drive(char),
    Walked(PathBuf),
}

pub(crate) fn search(
    engine: &mut Engine,
    request: &NamesRequest,
    report: &dyn Report,
) -> Result<NamesOutcome> {
    let query = NameQuery::new(&request.pattern, request.case);
    let roots = if request.roots.is_empty() {
        vec![PathBuf::from(".")]
    } else {
        request.roots.clone()
    };
    let cache = engine.cache.clone();
    let resident = engine.resident;
    let names = &mut engine.names;
    // Indexes caught up during this search.
    let mut current: HashSet<Source> = HashSet::new();
    let mut outcome = NamesOutcome::default();
    for typed in &roots {
        let joined = match &request.cwd {
            Some(cwd) => cwd.join(typed),
            None => typed.clone(),
        };
        let root = std::path::absolute(&joined).map_err(|source| io_error(typed, source))?;
        let (source, under) = if let Some((letter, under)) =
            drive_index(&root, cache.as_deref(), names, &mut current, report)
        {
            (Source::Drive(letter), Some(under))
        } else {
            walk_index(&root, cache.as_deref(), names, &mut current)?;
            (Source::Walked(root.clone()), None)
        };
        let loaded = match &source {
            Source::Drive(letter) => &names.drives[letter],
            Source::Walked(root) => &names.walked[root],
        };
        let index = &loaded.snapshot.index;
        let started = Instant::now();
        let mut found = index.search(&query, under, request.threads);
        // A drive index spells `root` as NTFS stores it: long names, stored case. Print
        // paths as joined onto the root the user typed (`RUNNER~1`, `c:\dev`), like rg.
        if let Some(stored) = under.and_then(|u| index.dir_path(u))
            && stored != root
        {
            respell(&mut found, &stored, &root);
        }
        outcome.stats.push(format!(
            "{}: {} matches in {:.1} ms; index of {} entries ({} MiB) from {} in {:.0} ms",
            root.display(),
            found.len(),
            ms(started.elapsed()),
            index.len(),
            index.memory_bytes() >> 20,
            loaded.how,
            ms(loaded.took),
        ));
        outcome.paths.extend(found);
    }
    if !resident {
        // Nothing to keep for the next search; what changed is saved by `Engine::save`.
        names.walked.retain(|_, l| l.save_to.is_some());
        names.drives.retain(|_, l| l.save_to.is_some());
    }
    Ok(outcome)
}

/// Saves name indexes that changed more than `quiet` ago (see [`Engine::save`]).
pub(crate) fn save(engine: &mut Engine, quiet: Duration) -> Vec<String> {
    let mut lines = Vec::new();
    for loaded in engine
        .names
        .drives
        .values_mut()
        .chain(engine.names.walked.values_mut())
    {
        if loaded.changed.elapsed() < quiet {
            continue;
        }
        let Some(path) = loaded.save_to.take() else {
            continue;
        };
        let started = Instant::now();
        match loaded.snapshot.save(&path) {
            Ok(()) => lines.push(format!(
                "saved {} in {:.0} ms",
                path.display(),
                ms(started.elapsed())
            )),
            Err(e) => lines.push(format!(
                "could not save the index to {}: {e}",
                path.display()
            )),
        }
    }
    if !engine.resident {
        engine.names = Names::default();
    }
    lines
}

/// Replaces the leading `stored` of every path with `root`. Every path from a search under
/// a directory starts with that directory's stored path, so their order is unchanged.
fn respell(paths: &mut [PathBuf], stored: &Path, root: &Path) {
    for path in paths {
        if let Ok(rest) = path.strip_prefix(stored) {
            *path = root.join(rest);
        }
    }
}

/// Catches up a name index held in memory. `false` if it can't be (the caller reloads).
#[cfg(windows)]
fn refresh(loaded: &mut Loaded, letter: char, path: Option<&Path>) -> bool {
    let started = Instant::now();
    let Ok(done) = mtseek_names::catch_up(
        &mut loaded.snapshot.index,
        letter,
        loaded.snapshot.checkpoint,
    ) else {
        return false;
    };
    loaded.snapshot.checkpoint = done.checkpoint;
    if done.changed {
        loaded.changed = Instant::now();
        loaded.save_to = path.map(Path::to_path_buf);
    }
    loaded.how = format!(
        "memory + {} journal records, {} changed dirs",
        done.records, done.changed_dirs
    );
    loaded.took = started.elapsed();
    true
}

/// Loads (catching up) or builds the whole-drive index for `root`'s drive. Returns the
/// drive letter and `root`'s id, or `None` if there is no such index and this process
/// can't build one (not elevated), so the caller walks instead.
#[cfg(windows)]
fn drive_index(
    root: &Path,
    cache: Option<&Path>,
    names: &mut Names,
    current: &mut HashSet<Source>,
    report: &dyn Report,
) -> Option<(char, FileId)> {
    use mtseek_ntfs::{VolumeError, file_ref};

    let letter = drive_letter(root)?;
    let under = file_ref(root).ok()?;
    let path = cache.map(|c| c.join(format!("names-{letter}.mft.idx")));
    if current.contains(&Source::Drive(letter)) {
        return Some((letter, under));
    }
    if let Some(loaded) = names.drives.get_mut(&letter) {
        if refresh(loaded, letter, path.as_deref()) {
            current.insert(Source::Drive(letter));
            return Some((letter, under));
        }
        names.drives.remove(&letter);
    }
    let started = Instant::now();
    if let Some(path) = &path
        && let Some(loaded) = caught_up(path, letter, Origin::Mft, None, started)
    {
        names.drives.insert(letter, loaded);
        current.insert(Source::Drive(letter));
        return Some((letter, under));
    }
    match mtseek_names::build_from_mft(letter) {
        Ok(built) => {
            let how = match built.method {
                mtseek_names::MftMethod::Raw => {
                    format!("NTFS MFT (raw records, {} unreadable)", built.bad_records)
                }
                mtseek_names::MftMethod::Enumeration => "NTFS MFT (enumeration)".to_owned(),
            };
            let snapshot = Snapshot {
                index: built.index,
                origin: Origin::Mft,
                checkpoint: built.checkpoint,
            };
            names.drives.insert(
                letter,
                Loaded {
                    snapshot,
                    how,
                    took: started.elapsed(),
                    save_to: path,
                    changed: Instant::now(),
                },
            );
            current.insert(Source::Drive(letter));
            Some((letter, under))
        }
        Err(VolumeError::NotElevated) => {
            report.note(
                "not running as administrator, so indexing by walking directories (slower the first time).",
            );
            None
        }
        Err(e) => {
            report.note(&format!("{e}; walking directories instead."));
            None
        }
    }
}

#[cfg(not(windows))]
fn drive_index(
    _: &Path,
    _: Option<&Path>,
    _: &mut Names,
    _: &mut HashSet<Source>,
    _: &dyn Report,
) -> Option<(char, FileId)> {
    None
}

/// Loads (catching up) or builds the walked index of exactly `root`.
fn walk_index(
    root: &Path,
    cache: Option<&Path>,
    names: &mut Names,
    current: &mut HashSet<Source>,
) -> Result<()> {
    let source = Source::Walked(root.to_path_buf());
    if current.contains(&source) {
        return Ok(());
    }
    let started = Instant::now();
    let path = cache.map(|c| c.join(format!("names-walk-{:016x}.idx", fnv1a(root))));
    let letter = drive_letter(root);
    #[cfg(windows)]
    if let Some(loaded) = names.walked.get_mut(root) {
        if letter.is_some_and(|letter| refresh(loaded, letter, path.as_deref())) {
            current.insert(source);
            return Ok(());
        }
        names.walked.remove(root);
    }
    if let (Some(path), Some(letter)) = (&path, letter)
        && let Some(loaded) = caught_up(path, letter, Origin::Walk, Some(root), started)
    {
        names.walked.insert(root.to_path_buf(), loaded);
        current.insert(source);
        return Ok(());
    }
    // Note the journal position *before* walking: whatever changes during the walk is
    // replayed by the next search.
    let checkpoint = letter.and_then(checkpoint_now);
    let (index, _) = mtseek_names::walk(root).map_err(|source| io_error(root, source))?;
    let save_to = path.filter(|_| checkpoint.is_some());
    names.walked.insert(
        root.to_path_buf(),
        Loaded {
            snapshot: Snapshot {
                index,
                origin: Origin::Walk,
                checkpoint: checkpoint.unwrap_or(mtseek_names::Checkpoint {
                    journal_id: 0,
                    usn: 0,
                }),
            },
            how: "directory walk".into(),
            took: started.elapsed(),
            save_to,
            changed: Instant::now(),
        },
    );
    current.insert(source);
    Ok(())
}

/// Loads the snapshot at `path` and applies the journal since it was saved. `None` if it
/// is missing, unreadable, for another root, or too old for the journal.
#[cfg(windows)]
fn caught_up(
    path: &Path,
    letter: char,
    origin: Origin,
    root: Option<&Path>,
    started: Instant,
) -> Option<Loaded> {
    /// Reading this many journal records again on every run costs more than a save.
    const RESAVE_AFTER_RECORDS: u64 = 50_000;

    let mut snapshot = Snapshot::load(path).ok()?;
    if snapshot.origin != origin || root.is_some_and(|r| snapshot.index.root_path() != r) {
        return None;
    }
    let loaded_in = started.elapsed();
    let done = mtseek_names::catch_up(&mut snapshot.index, letter, snapshot.checkpoint).ok()?;
    snapshot.checkpoint = done.checkpoint;
    let changed = done.changed || done.records > RESAVE_AFTER_RECORDS;
    Some(Loaded {
        how: format!(
            "{} snapshot (loaded in {:.0} ms) + {} journal records, {} changed dirs",
            if origin == Origin::Mft {
                "NTFS MFT"
            } else {
                "walk"
            },
            ms(loaded_in),
            done.records,
            done.changed_dirs,
        ),
        snapshot,
        took: started.elapsed(),
        save_to: changed.then(|| path.to_path_buf()),
        changed: Instant::now(),
    })
}

#[cfg(not(windows))]
fn caught_up(_: &Path, _: char, _: Origin, _: Option<&Path>, _: Instant) -> Option<Loaded> {
    None
}

#[cfg(windows)]
fn checkpoint_now(letter: char) -> Option<mtseek_names::Checkpoint> {
    mtseek_names::checkpoint_now(letter)
}

#[cfg(not(windows))]
fn checkpoint_now(_: char) -> Option<mtseek_names::Checkpoint> {
    None
}

/// `C:\...` or `\\?\C:\...` → `C`.
fn drive_letter(path: &Path) -> Option<char> {
    use std::path::{Component, Prefix};
    match path.components().next()? {
        Component::Prefix(p) => match p.kind() {
            Prefix::Disk(d) | Prefix::VerbatimDisk(d) => Some(char::from(d).to_ascii_uppercase()),
            _ => None,
        },
        _ => None,
    }
}

/// A stable 64-bit hash of a path, case-insensitive like Windows, for cache file names.
fn fnv1a(path: &Path) -> u64 {
    path.to_string_lossy()
        .to_lowercase()
        .bytes()
        .fold(0xcbf2_9ce4_8422_2325, |hash, b| {
            (hash ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
        })
}
