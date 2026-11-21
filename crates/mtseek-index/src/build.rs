//! Building a segment for a scope: list its files exactly as `rg --files` would, read them,
//! and write the segment.

use std::fs::File;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

use mtseek_core::{Error, Scope};
use mtseek_walk::Options;

use crate::format::VERSION;
use crate::{Content, Doc, IndexError, MAX_STORED, StoredError, WriteStats, text, write_segment};

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

/// Builds the segment for `scope` at `out`.
pub fn build(scope: &Scope, options: Options, out: &Path) -> Result<WriteStats, IndexError> {
    let key = scope_key(scope).map_err(|source| IndexError::Io {
        path: out.to_path_buf(),
        source,
    })?;
    let docs = Mutex::new(Vec::new());
    let errors = Mutex::new(Vec::new());
    let include_binary = scope.filter.include_binary;
    mtseek_walk::list_files(
        scope,
        options,
        &|root, path| {
            let relative = path
                .strip_prefix(&scope.roots[root])
                .unwrap_or(path)
                .to_path_buf();
            // The writer rejects a root it doesn't know, so an impossible overflow can't
            // go unnoticed.
            let root = u32::try_from(root).unwrap_or(u32::MAX);
            match read_stored(path, include_binary) {
                Ok(content) => lock(&docs).push(Doc {
                    root,
                    path: relative,
                    content,
                }),
                Err(e) => lock(&errors).push(StoredError {
                    root: Some(root),
                    path: relative,
                    message: e.to_string(),
                }),
            }
        },
        &|err| {
            let (path, message) = match err {
                Error::Io { path, source } => (path.clone(), source.to_string()),
                other => (PathBuf::new(), other.to_string()),
            };
            lock(&errors).push(StoredError {
                root: None,
                path,
                message,
            });
        },
    );
    let mut docs = docs.into_inner().unwrap_or_else(PoisonError::into_inner);
    let mut errors = errors.into_inner().unwrap_or_else(PoisonError::into_inner);
    errors.sort_by(|a, b| (a.root, &a.path).cmp(&(b.root, &b.path)));
    let roots = u32::try_from(scope.roots.len()).unwrap_or(u32::MAX);
    write_segment(out, &key, roots, &mut docs, &errors, options.thread_count())
}

/// Reads what the index stores for the file at `path` (see [`text::stored_len`]). When
/// binary files are skipped, reading stops at the first NUL, so big binaries cost little.
pub fn read_stored(path: &Path, include_binary: bool) -> io::Result<Content> {
    const CHUNK: u64 = 64 * 1024;
    let mut file = File::open(path)?;
    let len = file.metadata()?.len();
    if len > MAX_STORED {
        return Ok(Content::External);
    }
    let mut bytes = Vec::with_capacity(usize::try_from(len).unwrap_or(0));
    if include_binary {
        file.read_to_end(&mut bytes)?;
        return Ok(Content::Stored(bytes));
    }
    loop {
        let start = bytes.len();
        if (&mut file).take(CHUNK).read_to_end(&mut bytes)? == 0 {
            break;
        }
        // The first chunk is the whole file or a full CHUNK, so a BOM is always visible here.
        if start == 0 && text::has_utf16_bom(&bytes) {
            file.read_to_end(&mut bytes)?;
            break;
        }
        if let Some(i) = memchr::memchr(0, &bytes[start..]) {
            bytes.truncate(start + i + 1);
            break;
        }
    }
    if bytes.len() as u64 > MAX_STORED {
        return Ok(Content::External);
    }
    Ok(Content::Stored(bytes))
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}
