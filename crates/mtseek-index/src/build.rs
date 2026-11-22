//! Building a segment for a scope: list its files exactly as `rg --files` would, read them,
//! note every directory and ignore-control file with its file reference number (ADR 0011),
//! and write the segment.

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

use mtseek_core::{Error, Filter, Scope};
use mtseek_walk::{Listed, Options};

use crate::format::VERSION;
use crate::{
    Checkpoint, Content, Doc, Entry, IndexError, MAX_STORED, Meta, Sd, SegmentInfo, StoredError,
    WriteStats, membership, no_sd, ntfs, text, write_segment,
};

/// The filter an index is built with for a search with `filter` (ADR 0012): the same
/// ignore-file and hidden settings, binary files skipped (binary-mode searches read cut files
/// from disk) and links not followed (such searches never use an index).
pub fn coverage(filter: Filter) -> Filter {
    Filter {
        include_binary: false,
        follow_links: false,
        ..filter
    }
}

/// Describes an index of `scope` for the segment header: format version, coverage filter
/// and canonical roots. An index is used only by searches whose key it carries.
///
/// Fails if a root doesn't exist or isn't a directory.
pub fn scope_key(scope: &Scope) -> io::Result<String> {
    let f = coverage(scope.filter);
    let mut key = format!(
        "mtseek content index v{VERSION}\nignore files={} hidden={}\n",
        f.respect_ignore_files, f.include_hidden
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

/// Builds the base segment for `scope` at `out`, with the [`coverage`] of its filter.
///
/// The journal checkpoints are taken before walking, so whatever changes during the build
/// is replayed by the first catch-up.
pub fn build(scope: &Scope, options: Options, out: &Path) -> Result<WriteStats, IndexError> {
    let key = scope_key(scope).map_err(|source| IndexError::Io {
        path: out.to_path_buf(),
        source,
    })?;
    let scope = &Scope {
        roots: scope.roots.clone(),
        filter: coverage(scope.filter),
    };
    let checkpoints = checkpoints(scope);
    let all = membership::covered(&scope.filter);
    let mut found = Found::default();
    for root in 0..scope.roots.len() {
        found.extend(walk_tree(scope, root, Path::new(""), all, options));
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
/// are relative to the scope root. Files, directories and errors carry their membership
/// bits ([`membership`]).
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

/// The other walks an index with `filter` records membership for: every covered filter
/// except `filter`'s own.
fn narrower(filter: &Filter) -> impl Iterator<Item = u8> {
    let main = membership::bit(filter);
    let covered = membership::covered(filter);
    [
        membership::DEFAULT,
        membership::NO_IGNORE,
        membership::HIDDEN,
        membership::ALL,
    ]
    .into_iter()
    .filter(move |&b| b != main && covered & b != 0)
}

/// What one walk lists: paths relative to the root, and the errors it reports.
#[derive(Default)]
struct Listing {
    paths: HashSet<PathBuf>,
    errors: HashSet<(Option<u32>, PathBuf, String)>,
}

/// Lists `start` (relative to root `root`) with `filter`, to `max_depth`, without reading
/// files.
fn listing(
    scope: &Scope,
    root: usize,
    start: &Path,
    filter: Filter,
    options: Options,
    max_depth: Option<usize>,
) -> Listing {
    let root_path = &scope.roots[root];
    let walk_root = root_path.join(start);
    let sub = Scope {
        roots: vec![walk_root.clone()],
        filter,
    };
    let root_u32 = u32::try_from(root).unwrap_or(u32::MAX);
    let out = Mutex::new(Listing::default());
    mtseek_walk::list(
        &sub,
        options,
        max_depth,
        &|entry: Listed<'_>| {
            let rel = relative(entry.path, &walk_root, start);
            lock(&out).paths.insert(rel);
        },
        &|err| {
            let e = walk_error(err, root_path, root_u32);
            lock(&out).errors.insert((e.root, e.path, e.message));
        },
    );
    out.into_inner().unwrap_or_else(PoisonError::into_inner)
}

/// `path` (under `walk_root`, which is `start` below the root) relative to the root.
fn relative(path: &Path, walk_root: &Path, start: &Path) -> PathBuf {
    match path.strip_prefix(walk_root) {
        Ok(rest) if rest.as_os_str().is_empty() => start.to_path_buf(),
        Ok(rest) => start.join(rest),
        Err(_) => path.to_path_buf(),
    }
}

/// Sets the membership bits of everything in `found`: the bit of each narrower walk that
/// lists it, limited to `within` (the bits of the walk's starting folder). Errors that only a
/// narrower walk reports are added.
fn mark(found: &mut Found, walks: &[(u8, Listing)], within: u8) {
    for (bit, walk) in walks {
        let has = |path: &Path| walk.paths.contains(path);
        for doc in &mut found.docs {
            if has(&doc.path) {
                doc.flags |= bit;
            }
        }
        for dir in &mut found.dirs {
            if has(&dir.path) {
                dir.flags |= bit;
            }
        }
        for e in &mut found.errors {
            let key = (e.root, e.path.clone(), e.message.clone());
            if (e.root.is_some() && has(&e.path)) || walk.errors.contains(&key) {
                e.flags |= bit;
            }
        }
    }
    let mut extra: HashMap<(Option<u32>, PathBuf, String), u8> = HashMap::new();
    for (bit, walk) in walks {
        for key in &walk.errors {
            let known = found
                .errors
                .iter()
                .any(|e| e.root == key.0 && e.path == key.1 && e.message == key.2);
            if !known {
                *extra.entry(key.clone()).or_default() |= bit;
            }
        }
    }
    found.errors.extend(
        extra
            .into_iter()
            .map(|((root, path, message), flags)| StoredError {
                root,
                path,
                message,
                flags,
            }),
    );
    for doc in &mut found.docs {
        doc.flags &= within;
    }
    for dir in &mut found.dirs {
        dir.flags &= within;
    }
    for e in &mut found.errors {
        e.flags &= within;
    }
    found.errors.retain(|e| e.flags != 0);
}

/// Walks the subtree `start` (relative to root `root`) as ripgrep would with the scope's
/// filter, reading every file and listing every directory it enters. Then lists it with every
/// narrower filter the index covers, to record membership. `within` is the membership of
/// `start` itself ([`membership::covered`] for a root).
pub(crate) fn walk_tree(
    scope: &Scope,
    root: usize,
    start: &Path,
    within: u8,
    options: Options,
) -> Found {
    let root_path = &scope.roots[root];
    let walk_root = root_path.join(start);
    let sub = Scope {
        roots: vec![walk_root.clone()],
        filter: scope.filter,
    };
    // The writer rejects a root it doesn't know, so an impossible overflow can't go unnoticed.
    let root_u32 = u32::try_from(root).unwrap_or(u32::MAX);
    let main = membership::bit(&scope.filter);
    let found = Mutex::new(Found::default());
    let include_binary = scope.filter.include_binary;
    mtseek_walk::list(
        &sub,
        options,
        None,
        &|entry: Listed<'_>| {
            let rel = relative(entry.path, &walk_root, start);
            if entry.is_dir {
                let listing = ntfs::list_dir(entry.path).unwrap_or_default();
                let mut controls: Vec<Entry> = listing
                    .entries
                    .iter()
                    .filter(|e| ntfs::is_control(&e.name))
                    .map(|e| Entry {
                        frn: e.frn,
                        root: root_u32,
                        path: rel.join(&e.name),
                        flags: 0,
                        sd: ntfs::descriptor_at(&entry.path.join(&e.name)),
                    })
                    .collect();
                if listing
                    .entries
                    .iter()
                    .any(|e| e.name.eq_ignore_ascii_case(".git"))
                {
                    controls.extend(git_info(entry.path, &rel, root_u32));
                }
                let sd = ntfs::descriptor_at(entry.path);
                let mut found = lock(&found);
                found.dirs.push(Entry {
                    frn: listing.frn,
                    root: root_u32,
                    path: rel,
                    flags: main,
                    sd,
                });
                found.controls.extend(controls);
                return;
            }
            let read = read_file(entry.path, include_binary);
            let mut found = lock(&found);
            match read {
                Ok((content, frn, sd)) => found.docs.push(Doc {
                    root: root_u32,
                    path: rel,
                    frn,
                    flags: main,
                    content,
                    sd,
                }),
                Err(e) => found.errors.push(StoredError {
                    root: Some(root_u32),
                    path: rel,
                    message: e.to_string(),
                    flags: main,
                }),
            }
        },
        &|err| {
            let mut error = walk_error(err, root_path, root_u32);
            error.flags = main;
            lock(&found).errors.push(error);
        },
    );
    let mut found = found.into_inner().unwrap_or_else(PoisonError::into_inner);
    if start.as_os_str().is_empty() {
        found.controls.extend(above(root_path, root_u32));
    }
    let walks: Vec<(u8, Listing)> = narrower(&scope.filter)
        .map(|bit| {
            let filter = membership::filter(bit);
            (bit, listing(scope, root, start, filter, options, None))
        })
        .collect();
    mark(&mut found, &walks, within);
    found
}

/// The `.git\info` folder of the repository at `dir` (relative path `rel`) and its
/// `exclude` file, as control entries: `exclude` holds ignore rules the walk honours.
/// `.git` itself is not watched beyond its own entry, because git writes there constantly.
pub(crate) fn git_info(dir: &Path, rel: &Path, root: u32) -> Vec<Entry> {
    let info = rel.join(".git").join("info");
    let Ok(listing) = ntfs::list_dir(&dir.join(".git").join("info")) else {
        return Vec::new();
    };
    let info_dir = dir.join(".git").join("info");
    let mut out = vec![Entry {
        frn: listing.frn,
        root,
        path: info.clone(),
        flags: 0,
        sd: no_sd(),
    }];
    out.extend(
        listing
            .entries
            .iter()
            .filter(|e| e.name.eq_ignore_ascii_case("exclude"))
            .map(|e| Entry {
                frn: e.frn,
                root,
                path: info.join(&e.name),
                flags: 0,
                sd: ntfs::descriptor_at(&info_dir.join(&e.name)),
            }),
    );
    out
}

/// The folders above the root and their control files (and `.git\info` contents), as
/// control entries whose paths start with `..`: the walk reads the ignore files of every
/// folder above its root (ADR 0012).
fn above(root_path: &Path, root: u32) -> Vec<Entry> {
    let mut out = Vec::new();
    let mut rel = PathBuf::new();
    for dir in root_path.ancestors().skip(1) {
        rel.push("..");
        let Ok(listing) = ntfs::list_dir(dir) else {
            continue;
        };
        out.push(Entry {
            frn: listing.frn,
            root,
            path: rel.clone(),
            flags: 0,
            sd: no_sd(),
        });
        for e in listing.entries.iter().filter(|e| ntfs::is_control(&e.name)) {
            out.push(Entry {
                frn: e.frn,
                root,
                path: rel.join(&e.name),
                flags: 0,
                sd: ntfs::descriptor_at(&dir.join(&e.name)),
            });
            if e.name.eq_ignore_ascii_case(".git") {
                out.extend(git_info(dir, &rel, root));
            }
        }
    }
    out
}

/// Whether a control entry's path is above the root (starts with `..`).
pub(crate) fn is_above(path: &Path) -> bool {
    matches!(
        path.components().next(),
        Some(std::path::Component::ParentDir)
    )
}

/// For a control entry inside a `.git\info` folder, or that folder itself: the folder of
/// the repository it belongs to. `None` for other control entries.
pub(crate) fn repo_of(path: &Path) -> Option<&Path> {
    let names: Vec<&std::ffi::OsStr> = path.iter().collect();
    let is = |i: usize, name: &str| {
        names
            .get(i)
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.eq_ignore_ascii_case(name))
    };
    let n = names.len();
    if n >= 2 && is(n - 2, ".git") && is(n - 1, "info") {
        path.parent()?.parent()
    } else if n >= 3 && is(n - 3, ".git") && is(n - 2, "info") {
        path.parent()?.parent()?.parent()
    } else {
        None
    }
}

/// The direct entries of directory `dir` (relative to root `root`) that ripgrep's walker
/// would include: files and subdirectories, as paths relative to the root with their
/// membership bits, plus any errors.
pub(crate) struct Level {
    pub(crate) files: Vec<(PathBuf, u8)>,
    pub(crate) dirs: Vec<(PathBuf, u8)>,
    pub(crate) errors: Vec<StoredError>,
}

/// Lists directory `dir` one level deep with the scope's filter and every narrower one.
/// `within` is the membership of `dir` itself.
pub(crate) fn list_level(scope: &Scope, root: usize, dir: &Path, within: u8) -> Level {
    let root_path = &scope.roots[root];
    let walk_root = root_path.join(dir);
    let sub = Scope {
        roots: vec![walk_root.clone()],
        filter: scope.filter,
    };
    let root_u32 = u32::try_from(root).unwrap_or(u32::MAX);
    let main = membership::bit(&scope.filter);
    let found = Mutex::new(Found::default());
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
            let mut found = lock(&found);
            // Directories go to `dirs`, files to `docs` (content unused here).
            if entry.is_dir {
                found.dirs.push(Entry {
                    frn: 0,
                    root: root_u32,
                    path: rel,
                    flags: main,
                    sd: no_sd(),
                });
            } else {
                found.docs.push(Doc {
                    root: root_u32,
                    path: rel,
                    frn: 0,
                    flags: main,
                    content: Content::External,
                    sd: no_sd(),
                });
            }
        },
        &|err| {
            let mut error = walk_error(err, root_path, root_u32);
            error.flags = main;
            lock(&found).errors.push(error);
        },
    );
    let mut found = found.into_inner().unwrap_or_else(PoisonError::into_inner);
    let walks: Vec<(u8, Listing)> = narrower(&scope.filter)
        .map(|bit| {
            let filter = membership::filter(bit);
            let mut walk = listing(scope, root, dir, filter, Options { threads: 1 }, Some(1));
            walk.paths.remove(dir);
            (bit, walk)
        })
        .collect();
    mark(&mut found, &walks, within);
    Level {
        files: found.docs.into_iter().map(|d| (d.path, d.flags)).collect(),
        dirs: found.dirs.into_iter().map(|d| (d.path, d.flags)).collect(),
        errors: found.errors,
    }
}

/// Whether the contents of a file named `name` never go into the index (ADR 0004 and 0012
/// §4): environment files, private keys, certificates with keys, password vaults. Searches
/// read such files from disk, so results don't change.
pub fn is_secret(name: &str) -> bool {
    const PREFIXES: [&str; 5] = [".env", "id_rsa", "id_dsa", "id_ecdsa", "id_ed25519"];
    const SUFFIXES: [&str; 7] = [".pem", ".key", ".pfx", ".p12", ".kdbx", ".jks", ".keystore"];
    let name = name.to_ascii_lowercase();
    PREFIXES.iter().any(|p| name.starts_with(p)) || SUFFIXES.iter().any(|s| name.ends_with(s))
}

/// Whether a file with these attributes must not be read in the background: EFS-encrypted
/// (a service can't decrypt it), or a cloud placeholder whose data isn't on disk (reading
/// would download it).
fn not_to_read(attributes: u32) -> bool {
    const OFFLINE: u32 = 0x1000;
    const ENCRYPTED: u32 = 0x4000;
    const RECALL_ON_DATA_ACCESS: u32 = 0x40_0000;
    attributes & (OFFLINE | ENCRYPTED | RECALL_ON_DATA_ACCESS) != 0
}

#[cfg(windows)]
fn attributes(meta: &std::fs::Metadata) -> u32 {
    std::os::windows::fs::MetadataExt::file_attributes(meta)
}

#[cfg(not(windows))]
fn attributes(_: &std::fs::Metadata) -> u32 {
    0
}

/// Reads what the index stores for the file at `path` (see [`text::stored_len`]), its file
/// reference number and its security descriptor. When binary files are skipped, reading
/// stops at the first NUL, so big binaries cost little. Secret files ([`is_secret`]) aren't
/// read, nor are files that mustn't be read in the background (encrypted ones, cloud
/// placeholders): their contents stay on disk.
pub fn read_file(path: &Path, include_binary: bool) -> io::Result<(Content, u64, Sd)> {
    const CHUNK: u64 = 64 * 1024;
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(e) => {
            // An encrypted file another account owns can't be opened for reading at all.
            if e.kind() == io::ErrorKind::PermissionDenied
                && std::fs::symlink_metadata(path).is_ok_and(|m| not_to_read(attributes(&m)))
            {
                return Ok((Content::Unstored, 0, ntfs::descriptor_at(path)));
            }
            return Err(e);
        }
    };
    let frn = ntfs::file_frn(&file);
    let sd = ntfs::descriptor_of(&file);
    let meta = file.metadata()?;
    if not_to_read(attributes(&meta)) {
        return Ok((Content::Unstored, frn, sd));
    }
    let len = meta.len();
    let secret = path
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(is_secret);
    if len > MAX_STORED || secret {
        return Ok((Content::External, frn, sd));
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
        return Ok((Content::External, frn, sd));
    }
    Ok((Content::Stored(bytes), frn, sd))
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
            flags: 0,
        },
        Err(_) => StoredError {
            root: None,
            path,
            message,
            flags: 0,
        },
    }
}

pub(crate) fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}
