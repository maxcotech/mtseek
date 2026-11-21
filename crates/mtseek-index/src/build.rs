//! Building a segment for a scope: list its files exactly as `rg --files` would, read them,
//! note every directory and ignore-control file with its file reference number (ADR 0011),
//! and write the segment.

use std::fs::File;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

use mtseek_core::{Error, Scope};
use mtseek_walk::{Listed, Options};

use crate::format::VERSION;
use crate::{
    Checkpoint, Content, Doc, Entry, IndexError, MAX_STORED, Meta, SegmentInfo, StoredError,
    WriteStats, ntfs, text, write_segment,
};

/// Describes `scope` for the segment header: format version, filter and canonical roots.
/// A segment answers a query only if its key equals the query scope's key.
///
/// Fails if a root doesn't exist or isn't a directory.
pub fn scope_key(scope: &Scope) -> io::Result<String> {
    let f = scope.filter;
    let mut key = format!(
        "mtseek content index v{VERSION}\nignore files={} hidden={} binary={} follow={}\n",
        f.respect_ignore_files, f.include_hidden, f.include_binary, f.follow_links
    );
    for root in &scope.roots {
        let canonical = std::fs::canonicalize(root)?;
        if !canonical.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "{}: the content index covers directories only",
                    root.display()
                ),
            ));
        }
        key.push_str("root=");
        key.push_str(&canonical.to_string_lossy());
        key.push('\n');
    }
    Ok(key)
}

/// Builds the base segment for `scope` at `out`.
///
/// The journal checkpoints are taken before walking, so whatever changes during the build
/// is replayed by the first catch-up. A scope that follows links gets none: its files can be
/// anywhere, so it can't be caught up (ADR 0011).
pub fn build(scope: &Scope, options: Options, out: &Path) -> Result<WriteStats, IndexError> {
    let key = scope_key(scope).map_err(|source| IndexError::Io {
        path: out.to_path_buf(),
        source,
    })?;
    let checkpoints = if scope.filter.follow_links {
        Vec::new()
    } else {
        checkpoints(scope)
    };
    let mut found = Found::default();
    for root in 0..scope.roots.len() {
        found.extend(walk_tree(scope, root, Path::new(""), options));
    }
    found.sort();
    let meta = Meta {
        build_id: new_build_id(),
        base_id: 0,
        checkpoints,
    };
    let info = SegmentInfo {
        key: &key,
        roots: u32::try_from(scope.roots.len()).unwrap_or(u32::MAX),
        errors: &found.errors,
        dirs: &found.dirs,
        controls: &found.controls,
        tombstones: &[],
        meta: &meta,
    };
    write_segment(out, &info, &mut found.docs, options.thread_count())
}

/// The current journal position of every drive under the scope's roots. Empty unless every
/// root is on a drive with a readable journal.
pub(crate) fn checkpoints(scope: &Scope) -> Vec<Checkpoint> {
    let mut out: Vec<Checkpoint> = Vec::new();
    for root in &scope.roots {
        let Some(letter) = std::fs::canonicalize(root)
            .ok()
            .and_then(|p| ntfs::drive_letter(&p))
        else {
            return Vec::new();
        };
        if out.iter().any(|c| c.letter == letter) {
            continue;
        }
        match ntfs::checkpoint(letter) {
            Some(c) => out.push(c),
            None => return Vec::new(),
        }
    }
    out
}

/// A segment id that is unique in practice: the time in nanoseconds mixed with the process id.
pub(crate) fn new_build_id() -> u64 {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    // Keeping the low 64 bits of the nanosecond count is the point.
    #[allow(clippy::cast_possible_truncation)]
    let nanos = nanos as u64;
    (nanos ^ (u64::from(std::process::id()) << 40)).max(1)
}

/// What a walk found: files with their bytes, directories, control files and errors. Paths
/// are relative to the scope root.
#[derive(Debug, Default)]
pub(crate) struct Found {
    pub(crate) docs: Vec<Doc>,
    pub(crate) dirs: Vec<Entry>,
    pub(crate) controls: Vec<Entry>,
    pub(crate) errors: Vec<StoredError>,
}

impl Found {
    fn extend(&mut self, other: Found) {
        self.docs.extend(other.docs);
        self.dirs.extend(other.dirs);
        self.controls.extend(other.controls);
        self.errors.extend(other.errors);
    }

    fn sort(&mut self) {
        self.dirs.sort();
        self.controls.sort();
        self.errors
            .sort_by(|a, b| (a.root, &a.path).cmp(&(b.root, &b.path)));
    }
}

/// Walks the subtree `start` (relative to root `root`) as ripgrep would, reading every file
/// and listing every directory it enters.
pub(crate) fn walk_tree(scope: &Scope, root: usize, start: &Path, options: Options) -> Found {
    let root_path = &scope.roots[root];
    let walk_root = root_path.join(start);
    let sub = Scope {
        roots: vec![walk_root.clone()],
        filter: scope.filter,
    };
    // The writer rejects a root it doesn't know, so an impossible overflow can't go unnoticed.
    let root_u32 = u32::try_from(root).unwrap_or(u32::MAX);
    let relative = |path: &Path| -> PathBuf {
        match path.strip_prefix(&walk_root) {
            Ok(rest) if rest.as_os_str().is_empty() => start.to_path_buf(),
            Ok(rest) => start.join(rest),
            Err(_) => path.to_path_buf(),
        }
    };
    let found = Mutex::new(Found::default());
    let include_binary = scope.filter.include_binary;
    mtseek_walk::list(
        &sub,
        options,
        None,
        &|entry: Listed<'_>| {
            let rel = relative(entry.path);
            if entry.is_dir {
                let listing = ntfs::list_dir(entry.path).unwrap_or_default();
                let controls: Vec<Entry> = listing
                    .entries
                    .iter()
                    .filter(|e| ntfs::is_control(&e.name))
                    .map(|e| Entry {
                        frn: e.frn,
                        root: root_u32,
                        path: rel.join(&e.name),
                    })
                    .collect();
                let mut found = lock(&found);
                found.dirs.push(Entry {
                    frn: listing.frn,
                    root: root_u32,
                    path: rel,
                });
                found.controls.extend(controls);
                return;
            }
            let read = read_file(entry.path, include_binary);
            let mut found = lock(&found);
            match read {
                Ok((content, frn)) => found.docs.push(Doc {
                    root: root_u32,
                    path: rel,
                    frn,
                    content,
                }),
                Err(e) => found.errors.push(StoredError {
                    root: Some(root_u32),
                    path: rel,
                    message: e.to_string(),
                }),
            }
        },
        &|err| {
            let error = walk_error(err, root_path, root_u32);
            lock(&found).errors.push(error);
        },
    );
    found.into_inner().unwrap_or_else(PoisonError::into_inner)
}

/// The direct entries of directory `dir` (relative to root `root`) that ripgrep's walker
/// would include: files and subdirectories, as paths relative to the root, plus any errors.
pub(crate) struct Level {
    pub(crate) files: Vec<PathBuf>,
    pub(crate) dirs: Vec<PathBuf>,
    pub(crate) errors: Vec<StoredError>,
}

pub(crate) fn list_level(scope: &Scope, root: usize, dir: &Path) -> Level {
    let root_path = &scope.roots[root];
    let walk_root = root_path.join(dir);
    let sub = Scope {
        roots: vec![walk_root.clone()],
        filter: scope.filter,
    };
    let root_u32 = u32::try_from(root).unwrap_or(u32::MAX);
    let level = Mutex::new(Level {
        files: Vec::new(),
        dirs: Vec::new(),
        errors: Vec::new(),
    });
    mtseek_walk::list(
        &sub,
        Options { threads: 1 },
        Some(1),
        &|entry: Listed<'_>| {
            if entry.depth != 1 {
                return;
            }
            let Some(name) = entry.path.file_name() else {
                return;
            };
            let rel = dir.join(name);
            let mut level = lock(&level);
            if entry.is_dir {
                level.dirs.push(rel);
            } else {
                level.files.push(rel);
            }
        },
        &|err| {
            let error = walk_error(err, root_path, root_u32);
            lock(&level).errors.push(error);
        },
    );
    level.into_inner().unwrap_or_else(PoisonError::into_inner)
}

/// Reads what the index stores for the file at `path` (see [`text::stored_len`]), and its
/// file reference number. When binary files are skipped, reading stops at the first NUL, so
/// big binaries cost little.
pub fn read_file(path: &Path, include_binary: bool) -> io::Result<(Content, u64)> {
    const CHUNK: u64 = 64 * 1024;
    let mut file = File::open(path)?;
    let frn = ntfs::file_frn(&file);
    let len = file.metadata()?.len();
    if len > MAX_STORED {
        return Ok((Content::External, frn));
    }
    let mut bytes = Vec::with_capacity(usize::try_from(len).unwrap_or(0));
    if include_binary {
        file.read_to_end(&mut bytes)?;
    } else {
        loop {
            let start = bytes.len();
            if (&mut file).take(CHUNK).read_to_end(&mut bytes)? == 0 {
                break;
            }
            // The first chunk is the whole file or a full CHUNK, so a BOM is visible here.
            if start == 0 && text::has_utf16_bom(&bytes) {
                file.read_to_end(&mut bytes)?;
                break;
            }
            if let Some(i) = memchr::memchr(0, &bytes[start..]) {
                bytes.truncate(start + i + 1);
                break;
            }
        }
    }
    if bytes.len() as u64 > MAX_STORED {
        return Ok((Content::External, frn));
    }
    Ok((Content::Stored(bytes), frn))
}

/// A walk error as stored: relative to the root when it is under it.
fn walk_error(err: &Error, root_path: &Path, root: u32) -> StoredError {
    let (path, message) = match err {
        Error::Io { path, source } => (path.clone(), source.to_string()),
        other => (PathBuf::new(), other.to_string()),
    };
    match path.strip_prefix(root_path) {
        Ok(rel) => StoredError {
            root: Some(root),
            path: rel.to_path_buf(),
            message,
        },
        Err(_) => StoredError {
            root: None,
            path,
            message,
        },
    }
}

pub(crate) fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}
