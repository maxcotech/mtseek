//! `mtseek --names`: file-name search.
//!
//! Each run builds the index from scratch: from the NTFS master file table when elevated,
//! otherwise by walking directories. Keeping it built and live between runs is the daemon's
//! job (milestone M3); this mode measures and exercises the engine until then.

use std::collections::HashMap;
use std::io::{self, BufWriter, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use mtseek_core::{CaseMode, Error, Result};
use mtseek_names::{FileId, NameIndex, NameQuery};

use crate::Args;

/// Where an index came from, for `--stats`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Source {
    Mft,
    Walk,
}

struct Built {
    index: NameIndex,
    source: Source,
    took: Duration,
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

    // One MFT enumeration per drive, however many roots live on it.
    let mut volumes: HashMap<char, Built> = HashMap::new();
    let mut results = Vec::new();
    let mut stats = Vec::new();
    for root in &roots {
        let root = std::path::absolute(root).map_err(|source| io_error(root, source))?;
        let walked;
        let (built, under) = if let Some((letter, under)) = mft_for(&root, &mut volumes) {
            (&volumes[&letter], Some(under))
        } else {
            let started = Instant::now();
            let (index, _) = mtseek_names::walk(&root).map_err(|source| io_error(&root, source))?;
            walked = Built {
                index,
                source: Source::Walk,
                took: started.elapsed(),
            };
            (&walked, None)
        };
        let search_started = Instant::now();
        let found = built.index.search(&query, under, args.threads);
        stats.push(format!(
            "{}: {} matches in {:.1} ms; index from {} with {} entries ({} MiB) built in {:.0} ms",
            root.display(),
            found.len(),
            ms(search_started.elapsed()),
            if built.source == Source::Mft {
                "NTFS MFT"
            } else {
                "directory walk"
            },
            built.index.len(),
            built.index.memory_bytes() >> 20,
            ms(built.took),
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

/// Builds (or reuses) the MFT index for `root`'s drive. Returns the drive letter and the id
/// of `root` itself, or `None` to fall back to walking.
#[cfg(windows)]
fn mft_for(root: &Path, volumes: &mut HashMap<char, Built>) -> Option<(char, FileId)> {
    use mtseek_names::Kind;
    use mtseek_ntfs::{Volume, VolumeError, file_ref};

    let letter = drive_letter(root)?;
    let under = file_ref(root).ok()?;
    if volumes.contains_key(&letter) {
        return Some((letter, under));
    }
    let started = Instant::now();
    let volume = match Volume::open(letter) {
        Ok(volume) => volume,
        Err(VolumeError::NotElevated) => {
            hint(
                "not running as administrator, so walking directories instead of reading the NTFS index (slower).",
            );
            return None;
        }
        Err(e) => {
            hint(&format!("{e}; walking directories instead."));
            return None;
        }
    };
    let drive_root = PathBuf::from(format!(r"{letter}:\"));
    let root_id = file_ref(&drive_root).ok()?;
    let mut index = NameIndex::new(root_id, drive_root);
    let enumerated = volume.enumerate(&mut |r| {
        let kind = if r.is_dir() { Kind::Dir } else { Kind::File };
        index.upsert(r.file_ref, r.parent_ref, &r.name, kind);
    });
    if let Err(e) = enumerated {
        hint(&format!("{e}; walking directories instead."));
        return None;
    }
    volumes.insert(
        letter,
        Built {
            index,
            source: Source::Mft,
            took: started.elapsed(),
        },
    );
    Some((letter, under))
}

#[cfg(not(windows))]
fn mft_for(_: &Path, _: &mut HashMap<char, Built>) -> Option<(char, FileId)> {
    None
}

/// `C:\...` or `\\?\C:\...` → `C`.
#[cfg(windows)]
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

/// A one-line note on stderr, only when a person is watching (never pollutes pipes or agents).
fn hint(message: &str) {
    if io::stderr().is_terminal() {
        let _ = writeln!(io::stderr(), "mtseek: {message}");
    }
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

fn io_error(path: &Path, source: io::Error) -> Error {
    Error::Io {
        path: path.to_path_buf(),
        source,
    }
}
