//! Content search through the content index (ADR 0010, 0011).
//!
//! The index for a scope (its directories and filters) lives in the cache directory, next to
//! the name indexes. `--index` builds it the first time. From then on every content search of
//! that scope uses it: the run first applies what the NTFS change journal recorded since the
//! last run, so results are never stale, then searches the index without opening user files.
//! `--no-index` scans instead; `--reindex` rebuilds.

use std::io::{self, Write};
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use std::time::Instant;

use mtseek_core::{Error, Query, Result, Scope};
use mtseek_index::{CatchUp, Index};
use mtseek_walk::{FileHit, Mode, Options, Summary};

use crate::Args;
use crate::names::{cache_dir, hint, ms};

/// Searches through the scope's index. `None` if there is no index to use, so the caller
/// scans: no index yet and `--index` not given, `--no-index`, or an index that can't be kept
/// current (no change journal) without `--index` asking for a rebuild.
pub(crate) fn search(
    args: &Args,
    scope: &Scope,
    query: &Query,
    mode: Mode,
    options: Options,
    on_hit: &(dyn Fn(&FileHit<'_>) -> ControlFlow<()> + Sync),
    on_error: &(dyn Fn(&Error) + Sync),
) -> Result<Option<Summary>> {
    let explicit = args.index || args.reindex;
    if args.no_index {
        return Ok(None);
    }
    let Some(cache) = cache_dir() else {
        if explicit {
            return Err(Error::Io {
                path: PathBuf::from("<cache>"),
                source: io::Error::new(
                    io::ErrorKind::NotFound,
                    "no cache directory: set MTSEEK_CACHE_DIR or LOCALAPPDATA",
                ),
            });
        }
        return Ok(None);
    };
    let first_root = scope.roots.first().cloned().unwrap_or_default();
    let (path, key) = match mtseek_query::segment_path(&cache, scope) {
        Ok(found) => found,
        // A missing root or a file: let the scanner report it the way rg does.
        Err(_) if !explicit => return Ok(None),
        Err(source) => return Err(io_error(&first_root, source)),
    };

    let mut stats = Vec::new();
    let started = Instant::now();
    let existing = if args.reindex {
        None
    } else {
        Index::open(&path, &key)
    };
    let index = match existing {
        None if !explicit => return Ok(None),
        None => build(scope, options, &cache, &path, &mut stats, started)?,
        Some(index) if index.meta().checkpoints.is_empty() && !explicit => return Ok(None),
        Some(mut index) => {
            let opened = started.elapsed();
            match index.catch_up(scope, options)? {
                CatchUp::NeedsRebuild(reason) => {
                    hint(&format!("rebuilding the content index: {reason}"));
                    drop(index);
                    build(scope, options, &cache, &path, &mut stats, started)?
                }
                done => {
                    stats.push(format!(
                        "index of {} files ({:.1} MiB) opened in {:.1} ms, caught up in {:.1} ms: {}",
                        index.len(),
                        mib(index.file_bytes()),
                        ms(opened),
                        ms(started.elapsed().saturating_sub(opened)),
                        describe(&done),
                    ));
                    index
                }
            }
        }
    };
    stats.push(format!("index file {}", path.display()));

    let searched = Instant::now();
    let result = mtseek_query::search(&index, scope, query, mode, options, on_hit, on_error);
    let (summary, search_stats) = match result {
        Ok(done) => done,
        Err(err) => {
            if !matches!(err, Error::InvalidPattern(_)) {
                // The index itself failed: drop it, so the next run builds a fresh one.
                drop(index);
                let _ = std::fs::remove_file(&path);
                let _ = std::fs::remove_file(mtseek_index::delta_path(&path));
            }
            return Err(err);
        }
    };
    let mut plan = search_stats.plan.to_string();
    if plan.len() > 300 {
        let cut = (0..=300).rev().find(|&i| plan.is_char_boundary(i));
        plan.truncate(cut.unwrap_or(0));
        plan.push_str("...");
    }
    stats.push(format!(
        "{} candidates, {} blocks read, {} matched in {:.1} ms; plan {plan}",
        search_stats.candidates,
        search_stats.blocks,
        summary.files_matched,
        ms(searched.elapsed()),
    ));
    if args.stats {
        let mut err = io::stderr().lock();
        for line in stats {
            let _ = writeln!(err, "{line}");
        }
    }
    Ok(Some(summary))
}

fn build(
    scope: &Scope,
    options: Options,
    cache: &Path,
    path: &Path,
    stats: &mut Vec<String>,
    started: Instant,
) -> Result<Index> {
    std::fs::create_dir_all(cache).map_err(|source| io_error(cache, source))?;
    hint("building the content index for these paths and filters (only the first time)");
    let (index, built) = Index::build(scope, options, path)?;
    stats.push(format!(
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
