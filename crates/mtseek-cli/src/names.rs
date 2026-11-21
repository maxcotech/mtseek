//! `mtseek --names`: file-name search.
//!
//! The index is cached on disk (`%LOCALAPPDATA%\mtseek\cache`, or `MTSEEK_CACHE_DIR`) with
//! the change-journal position it reflects. A run loads it and applies only what changed
//! since, which needs no admin rights. Without a usable cache, a run builds the index: from
//! the NTFS master file table when elevated (the whole drive), otherwise by walking the
//! root. Keeping it in memory and live between runs is the daemon's job (milestone M3).

use std::collections::HashMap;
use std::io::{self, BufWriter, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use mtseek_core::{CaseMode, Error, Result};
use mtseek_names::{FileId, NameQuery, Origin, Snapshot};

use crate::Args;

/// An index ready to search, and what it took to get it.
struct Loaded {
    snapshot: Snapshot,
    /// How it was obtained, for `--stats`.
    how: String,
    took: Duration,
    /// Save it after answering: it was built or changed, and has a journal position.
    save_to: Option<PathBuf>,
}

pub(crate) fn run(args: &Args) -> Result<ExitCode> {
    let case = if args.ignore_case {
        CaseMode::Insensitive
    } else if args.case_sensitive {
        CaseMode::Sensitive
    } else {
        CaseMode::Smart
    };
    let query = NameQuery::new(&args.pattern, case);
    let roots = if args.paths.is_empty() {
        vec![PathBuf::from(".")]
    } else {
        args.paths.clone()
    };
    let cache = cache_dir();

    // Whole-drive (MFT) indexes, shared by every root on the drive.
    let mut drives: HashMap<char, Loaded> = HashMap::new();
    let mut walked: Vec<Loaded> = Vec::new();
    let mut results = Vec::new();
    let mut stats = Vec::new();
    for root in &roots {
        let root = std::path::absolute(root).map_err(|source| io_error(root, source))?;
        let (loaded, under) =
            if let Some((letter, under)) = drive_index(&root, cache.as_deref(), &mut drives) {
                (&drives[&letter], Some(under))
            } else {
                walked.push(walk_index(&root, cache.as_deref())?);
                (&walked[walked.len() - 1], None)
            };
        let index = &loaded.snapshot.index;
        let started = Instant::now();
        let found = index.search(&query, under, args.threads);
        stats.push(format!(
            "{}: {} matches in {:.1} ms; index of {} entries ({} MiB) from {} in {:.0} ms",
            root.display(),
            found.len(),
            ms(started.elapsed()),
            index.len(),
            index.memory_bytes() >> 20,
            loaded.how,
            ms(loaded.took),
        ));
        results.extend(found);
    }

    let mut out = BufWriter::with_capacity(64 * 1024, io::stdout().lock());
    let written = results.iter().try_for_each(|path| {
        out.write_all(path.as_os_str().to_string_lossy().as_bytes())?;
        out.write_all(b"\n")
    });
    // A closed pipe (`mtseek --names x | head`) is a normal way to stop, not a failure.
    if let Err(err) = written.and_then(|()| out.flush())
        && err.kind() != io::ErrorKind::BrokenPipe
    {
        return Err(io_error(Path::new("<stdout>"), err));
    }
    drop(out);

    // Save after answering, so the caller never waits for it. Failing to save only costs
    // the next run some time.
    for loaded in drives.values().chain(&walked) {
        if let Some(path) = &loaded.save_to {
            let started = Instant::now();
            match loaded.snapshot.save(path) {
                Ok(()) => stats.push(format!(
                    "saved {} in {:.0} ms",
                    path.display(),
                    ms(started.elapsed())
                )),
                Err(e) => hint(&format!(
                    "could not save the index to {}: {e}",
                    path.display()
                )),
            }
        }
    }
    if args.stats {
        let mut err = io::stderr().lock();
        for line in stats {
            let _ = writeln!(err, "{line}");
        }
    }
    Ok(if results.is_empty() {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    })
}

/// The cache directory: `MTSEEK_CACHE_DIR`, else `%LOCALAPPDATA%\mtseek\cache`.
pub(crate) fn cache_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("MTSEEK_CACHE_DIR") {
        return Some(dir.into());
    }
    let local = std::env::var_os("LOCALAPPDATA")?;
    Some(Path::new(&local).join("mtseek").join("cache"))
}

/// Loads (catching up) or builds the whole-drive index for `root`'s drive. Returns the
/// drive letter and `root`'s id, or `None` if there is no such index and this process
/// can't build one (not elevated), so the caller walks instead.
#[cfg(windows)]
fn drive_index(
    root: &Path,
    cache: Option<&Path>,
    drives: &mut HashMap<char, Loaded>,
) -> Option<(char, FileId)> {
    use mtseek_ntfs::{VolumeError, file_ref};

    let letter = drive_letter(root)?;
    let under = file_ref(root).ok()?;
    if drives.contains_key(&letter) {
        return Some((letter, under));
    }
    let path = cache.map(|c| c.join(format!("names-{letter}.mft.idx")));
    let started = Instant::now();
    if let Some(path) = &path
        && let Some(loaded) = caught_up(path, letter, Origin::Mft, None, started)
    {
        drives.insert(letter, loaded);
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
            drives.insert(
                letter,
                Loaded {
                    snapshot,
                    how,
                    took: started.elapsed(),
                    save_to: path,
                },
            );
            Some((letter, under))
        }
        Err(VolumeError::NotElevated) => {
            hint(
                "not running as administrator, so indexing by walking directories (slower the first time).",
            );
            None
        }
        Err(e) => {
            hint(&format!("{e}; walking directories instead."));
            None
        }
    }
}

#[cfg(not(windows))]
fn drive_index(
    _: &Path,
    _: Option<&Path>,
    _: &mut HashMap<char, Loaded>,
) -> Option<(char, FileId)> {
    None
}

/// Loads (catching up) or builds the walked index of exactly `root`.
fn walk_index(root: &Path, cache: Option<&Path>) -> Result<Loaded> {
    let started = Instant::now();
    let path = cache.map(|c| c.join(format!("names-walk-{:016x}.idx", fnv1a(root))));
    let letter = drive_letter(root);
    if let (Some(path), Some(letter)) = (&path, letter)
        && let Some(loaded) = caught_up(path, letter, Origin::Walk, Some(root), started)
    {
        return Ok(loaded);
    }
    // Note the journal position *before* walking: whatever changes during the walk is
    // replayed by the next run.
    let checkpoint = letter.and_then(checkpoint_now);
    let (index, _) = mtseek_names::walk(root).map_err(|source| io_error(root, source))?;
    let save_to = path.filter(|_| checkpoint.is_some());
    Ok(Loaded {
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
    })
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

/// A one-line note on stderr, only when a person is watching (never pollutes pipes or agents).
pub(crate) fn hint(message: &str) {
    if io::stderr().is_terminal() {
        let _ = writeln!(io::stderr(), "mtseek: {message}");
    }
}

pub(crate) fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

fn io_error(path: &Path, source: io::Error) -> Error {
    Error::Io {
        path: path.to_path_buf(),
        source,
    }
}
