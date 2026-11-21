//! `mtseek --index`: content search through the content index (ADR 0010, milestone M2a).
//!
//! The index for a scope (its directories and filters) lives in the cache directory, next to
//! the name indexes. The first `--index` search of a scope builds it. Later ones open it and
//! search it, reading no user file at all. It is not caught up with file changes yet (M2b),
//! which is why it is opt-in; `--reindex` rebuilds it.

use std::io::{self, Write};
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use std::time::Instant;

use mtseek_core::{Error, Query, Result, Scope};
use mtseek_index::Segment;
use mtseek_walk::{FileHit, Mode, Options, Summary};

use crate::Args;
use crate::names::{cache_dir, hint, ms};

pub(crate) fn search(
    args: &Args,
    scope: &Scope,
    query: &Query,
    mode: Mode,
    options: Options,
    on_hit: &(dyn Fn(&FileHit<'_>) -> ControlFlow<()> + Sync),
    on_error: &(dyn Fn(&Error) + Sync),
) -> Result<Summary> {
    let cache = cache_dir().ok_or_else(|| Error::Io {
        path: PathBuf::from("<cache>"),
        source: io::Error::new(
            io::ErrorKind::NotFound,
            "no cache directory: set MTSEEK_CACHE_DIR or LOCALAPPDATA",
        ),
    })?;
    std::fs::create_dir_all(&cache).map_err(|source| io_error(&cache, source))?;
    let first_root = scope.roots.first().cloned().unwrap_or_default();
    let (path, key) = mtseek_query::segment_path(&cache, scope)
        .map_err(|source| io_error(&first_root, source))?;

    let mut stats = Vec::new();
    let started = Instant::now();
    let existing = if args.reindex {
        None
    } else {
        Segment::open(&path).ok().filter(|s| s.key() == key)
    };
    let segment = if let Some(segment) = existing {
        stats.push(format!(
            "index of {} files ({:.1} MiB) opened in {:.1} ms",
            segment.len(),
            mib(segment.file_bytes()),
            ms(started.elapsed())
        ));
        segment
    } else {
        hint("building the content index for these paths and filters (only the first time)");
        let built = mtseek_index::build(scope, options, &path)?;
        let segment = Segment::open(&path)?;
        stats.push(format!(
            "index of {} files built in {:.0} ms: {:.1} MB of text stored as {:.1} MB, \
             {} trigrams, {} postings, {:.1} MiB on disk, {} unreadable",
            built.files,
            ms(started.elapsed()),
            megabytes(built.stored_bytes),
            megabytes(built.compressed_bytes),
            built.trigrams,
            built.postings,
            mib(built.file_bytes),
            segment.errors().len(),
        ));
        segment
    };
    stats.push(format!("index file {}", path.display()));

    let searched = Instant::now();
    let result = mtseek_query::search(&segment, scope, query, mode, options, on_hit, on_error);
    let (summary, search_stats) = match result {
        Ok(done) => done,
        Err(err) => {
            if !matches!(err, Error::InvalidPattern(_)) {
                // The index itself failed: drop it, so the next run builds a fresh one.
                drop(segment);
                let _ = std::fs::remove_file(&path);
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
    Ok(summary)
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
