//! Test support for mtseek.
//!
//! [`generate`] writes a deterministic synthetic corpus: the same seed always produces the
//! same bytes. Alongside ordinary source-like files it plants the cases that break search
//! tools: git and non-git `.gitignore`s, `.ignore` / `.rgignore`, hidden files, binary files
//! (NUL early and NUL after the first 64 KiB buffer), UTF-16 and UTF-8 BOMs, CRLF, missing
//! final newline, empty files, very long lines and non-ASCII case folding.
//!
//! The tokens in [`tokens`] are planted at known rates so queries span from no matches
//! to most files matching, like the real benchmark in `bench/`.

use std::fmt::Write as _;
use std::fs;
use std::io;
use std::path::Path;

/// Tokens planted in the corpus, by rough selectivity.
pub mod tokens {
    /// Never written anywhere.
    pub const ABSENT: &str = "zq7xkvq9plmw";
    /// About 1% of files.
    pub const RARE: &str = "Paystack";
    /// About 4% of files.
    pub const UNCOMMON: &str = "HttpClient";
    /// About 15% of files.
    pub const MEDIUM: &str = "useEffect";
    /// About 60% of files.
    pub const COMMON: &str = "import";
    /// Planted only in the edge-case files, so each special file is easy to find.
    pub const NEEDLE: &str = "needle";
}

/// What to generate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CorpusSpec {
    /// RNG seed. Same seed, same corpus.
    pub seed: u64,
    /// Number of ordinary (non-edge-case) files.
    pub files: usize,
}

impl Default for CorpusSpec {
    fn default() -> Self {
        Self {
            seed: 0x6d74_7365_656b,
            files: 400,
        }
    }
}

/// What was written.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CorpusStats {
    /// Files written, including ignore files and edge cases.
    pub files: usize,
    /// Total bytes written.
    pub bytes: u64,
}

/// Writes a corpus under `root` (which should be empty).
pub fn generate(root: &Path, spec: &CorpusSpec) -> io::Result<CorpusStats> {
    let mut w = Writer {
        root,
        stats: CorpusStats::default(),
    };
    let mut rng = Rng(spec.seed);

    write_layout(&mut w)?;
    write_edge_cases(&mut w)?;

    for i in 0..spec.files {
        let dir = ORDINARY_DIRS[rng.below(ORDINARY_DIRS.len())];
        let ext = EXTENSIONS[rng.below(EXTENSIONS.len())];
        let path = format!("{dir}/file{i:05}.{ext}");
        let body = ordinary_file(&mut rng);
        w.file(&path, body.as_bytes())?;
    }
    Ok(w.stats)
}

/// Directories that receive ordinary files. Some are ignored under some filters, which is the point.
const ORDINARY_DIRS: &[&str] = &[
    "app/src",
    "app/src/components",
    "app/tests",
    "app/build",            // ignored by app/.gitignore
    "app/node_modules/pkg", // ignored by app/.gitignore
    "app/generated",        // ignored by app/.ignore
    "app/vendor",           // ignored by app/.rgignore
    "service/src",
    "service/obj", // ignored by service/.gitignore
    "loose/notes", // loose/ is not a git repo, so its .gitignore does nothing
    "app/.cache",  // hidden
    "docs",
];

const EXTENSIONS: &[&str] = &["rs", "cs", "ts", "tsx", "md", "txt", "json", "log"];

const WORDS: &[&str] = &[
    "let", "const", "return", "if", "else", "for", "while", "match", "struct", "class", "public",
    "private", "async", "await", "fn", "value", "result", "error", "request", "response", "user",
    "account", "payment", "order", "config", "handler", "service", "client", "server", "data",
    "index", "query", "search", "file", "path", "string", "number", "true", "false", "null", "(",
    ")", "{", "}", "=", "=>", ";", ".", ",", "+", "-", "*", "//", "\"", "'", "[", "]",
];

fn write_layout(w: &mut Writer<'_>) -> io::Result<()> {
    // `.git` directories make `app/` and `service/` git repos; ignore files there apply.
    w.dir("app/.git")?;
    w.dir("service/.git")?;
    w.file("app/.gitignore", b"build/\nnode_modules/\n*.log\n")?;
    w.file("app/.ignore", b"generated/\n")?;
    w.file("app/.rgignore", b"vendor/\n")?;
    w.file("service/.gitignore", b"obj/\n")?;
    w.file("loose/.gitignore", b"notes/\n")?;
    Ok(())
}

fn write_edge_cases(w: &mut Writer<'_>) -> io::Result<()> {
    let n = tokens::NEEDLE;
    w.file("edge/plain.txt", format!("a {n} here\n").as_bytes())?;
    w.file(
        "edge/crlf.txt",
        format!("first\r\nsecond {n}\r\nthird {n}\r\n").as_bytes(),
    )?;
    w.file(
        "edge/no-final-newline.txt",
        format!("line one\nlast {n}").as_bytes(),
    )?;
    w.file("edge/empty.txt", b"")?;
    w.file(
        "edge/.hidden-file.txt",
        format!("{n} in a hidden file\n").as_bytes(),
    )?;
    w.file(
        "edge/.hidden-dir/inside.txt",
        format!("{n} in a hidden dir\n").as_bytes(),
    )?;

    // Binary: NUL in the first buffer, so the file is skipped entirely by default.
    w.file("edge/binary-early.bin", format!("\0\0{n}\n").as_bytes())?;
    // Binary: a match in the first 64 KiB buffer, the NUL ~200 KiB later. Lines stay short:
    // a line longer than the buffer makes the searcher grow it, and then whether the NUL is
    // seen before the match depends on read sizes, so even ripgrep's own output flips.
    let mut late = format!("{n} before the binary part\n").into_bytes();
    for _ in 0..(200 * 1024 / 64) {
        late.extend_from_slice(&[b'x'; 63]);
        late.push(b'\n');
    }
    late.extend(format!("\0\0 {n} after the NUL\n").as_bytes());
    w.file("edge/binary-late.bin", &late)?;

    // Encodings.
    let mut little_endian = vec![0xFF, 0xFE];
    little_endian.extend(
        format!("utf-16le {n}\n")
            .encode_utf16()
            .flat_map(u16::to_le_bytes),
    );
    w.file("edge/utf16le.txt", &little_endian)?;
    let mut big_endian = vec![0xFE, 0xFF];
    big_endian.extend(
        format!("utf-16be {n}\n")
            .encode_utf16()
            .flat_map(u16::to_be_bytes),
    );
    w.file("edge/utf16be.txt", &big_endian)?;
    w.file(
        "edge/utf8-bom.txt",
        format!("\u{FEFF}utf-8 bom {n}\n").as_bytes(),
    )?;

    // Unicode case folding: `-i café` must find CAFÉ; `-i straße` is a classic trap.
    w.file(
        "edge/unicode.txt",
        "Café\nCAFÉ\ncafé\nSTRASSE\nstraße\n日本語 needle\n".as_bytes(),
    )?;

    // A single 200 KiB line with the match at the end.
    let mut long = "y".repeat(200 * 1024);
    _ = writeln!(long, " {n}");
    w.file("edge/long-line.txt", long.as_bytes())?;

    // Regex fodder: phone numbers and ConnectionString variants.
    w.file(
        "edge/regex.txt",
        b"call 555-123-4567 now\nnot a phone 5551234567\nConnectionString=x\nConnection_String=y\nConnectionPoolString=z\n",
    )?;
    Ok(())
}

fn ordinary_file(rng: &mut Rng) -> String {
    let mut out = String::new();
    if rng.chance(60) {
        out.push_str("import { thing } from \"./thing\";\n");
    }
    let lines = 5 + rng.below(60);
    for _ in 0..lines {
        let words = 1 + rng.below(12);
        for i in 0..words {
            if i > 0 {
                out.push(' ');
            }
            out.push_str(WORDS[rng.below(WORDS.len())]);
        }
        out.push('\n');
    }
    // Plant each token at most once per file, at its target rate.
    for (token, percent) in [
        (tokens::RARE, 1),
        (tokens::UNCOMMON, 4),
        (tokens::MEDIUM, 15),
    ] {
        if rng.chance(percent) {
            _ = writeln!(out, "const x = {token}.call();");
        }
    }
    if rng.chance(2) {
        out.push_str("var connectionString = ConnectionString;\n");
    }
    if rng.chance(1) {
        _ = writeln!(
            out,
            "phone: {:03}-{:03}-{:04}",
            rng.below(1000),
            rng.below(1000),
            rng.below(10_000)
        );
    }
    out
}

struct Writer<'a> {
    root: &'a Path,
    stats: CorpusStats,
}

impl Writer<'_> {
    fn dir(&mut self, rel: &str) -> io::Result<()> {
        fs::create_dir_all(self.root.join(rel))
    }

    fn file(&mut self, rel: &str, bytes: &[u8]) -> io::Result<()> {
        let path = self.root.join(rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&path, bytes)?;
        self.stats.files += 1;
        self.stats.bytes += bytes.len() as u64;
        Ok(())
    }
}

/// `SplitMix64`: tiny, fast, and stable forever (unlike `rand`'s default generators),
/// so a seed in a bug report reproduces the same corpus years later.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    #[allow(clippy::cast_possible_truncation)] // the result is < n, which is a usize
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }

    fn chance(&mut self, percent: u64) -> bool {
        self.next() % 100 < percent
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(root: &Path) -> Vec<(String, Vec<u8>)> {
        let mut out = Vec::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            for entry in fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    stack.push(path);
                } else {
                    let rel = path
                        .strip_prefix(root)
                        .unwrap()
                        .to_string_lossy()
                        .into_owned();
                    out.push((rel, fs::read(&path).unwrap()));
                }
            }
        }
        out.sort();
        out
    }

    #[test]
    fn same_seed_same_bytes() {
        let (a, b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let spec = CorpusSpec { seed: 7, files: 50 };
        assert_eq!(
            generate(a.path(), &spec).unwrap(),
            generate(b.path(), &spec).unwrap()
        );
        assert_eq!(snapshot(a.path()), snapshot(b.path()));
    }

    #[test]
    fn different_seed_different_bytes() {
        let (a, b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        generate(a.path(), &CorpusSpec { seed: 1, files: 50 }).unwrap();
        generate(b.path(), &CorpusSpec { seed: 2, files: 50 }).unwrap();
        assert_ne!(snapshot(a.path()), snapshot(b.path()));
    }
}
