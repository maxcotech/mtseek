# 0010: Content index: what is stored, how queries are planned, and the segment file

**Status:** Accepted, 2026-11-21. Makes [0001](0001-trigram-index.md) and
[0002](0002-content-store.md) concrete for milestone M2.

## Context

0001 settled on a trigram index and 0002 on storing compressed file text. Before writing code,
three things still had to be pinned down. Each one decides whether the index can answer
exactly what ripgrep answers (invariant 2) rather than nearly the same:

1. which bytes to store, so the verifier sees what ripgrep's searcher sees
2. how to turn any pattern into a trigram query without ever missing a match
3. the on-disk layout, and how a CLI without a daemon gets an index

Measured on the `dev` scope (15,385 files, 693 MB on disk) with a throwaway program:

| | |
|---|---:|
| stored text, binary files cut after their first NUL (see below) | 102.6 MB |
| zstd level 1 / 3 / 9 | 21.6 / **20.5** / 18.7 MB |
| lz4 | 30.3 MB |
| decompression on one thread, zstd 3 / lz4 | **1.24** / 1.51 GB/s |
| distinct ASCII-folded trigrams | 121,330 |
| postings (one per distinct trigram per file) | 14.3 M |

## Decision

### What is indexed and stored

- **One index per scope:** an ordered list of roots, plus the filter (ignore files, hidden,
  binary). Its file list comes from the same `ignore` walker the reference scanner uses
  (`mtseek-walk`), so the indexed files are exactly what `rg --files` lists for those flags.
  Every root must be a directory.
- **The store holds raw file bytes, not transcoded text.** The verifier runs the reference
  scanner's own `grep_searcher::Searcher` over them with `search_reader`.
  - With memory maps off, as in `mtseek-walk` and as `rg` does for directory walks,
    `search_path` is `File::open` followed by that same `search_reader`. BOM sniffing,
    UTF-16 transcoding, binary detection and line numbering therefore run the same code on
    the same bytes.
- **Binary files are cut after their first NUL** when the filter skips binary files (the
  default, and `-u`/`-uu`). In that mode the searcher's line buffer sets its end at the first
  NUL and never reads further (`grep-searcher` 0.1.17, `line_buffer.rs`, `fill`). So
  `bytes[..=first_nul]` searches identically, warning offset included.
  - The exception is a file starting with a UTF-16 BOM: its NULs are measured after decoding,
    so it is stored whole.
  - With `-uuu` every file is stored whole.
  - The cut is what keeps the `dev` store at 103 MB instead of 693 MB.
- **Trigrams** are every distinct 3-byte window of the text the matcher sees. That is the
  raw stored bytes, or the decoded UTF-8 for UTF-16 files, decoded with grep's own
  `encoding_rs_io` settings. Each byte is ASCII-lowercased first. Trigrams that cross line
  ends or come from a UTF-8 BOM are harmless extras: the index only has to over-report.
- **Files that can't be read** at build time are recorded with their error and reported again
  by every query (exit code 2), as `rg` would.

### Planning

- The pattern is parsed with `regex-syntax`.
  - Case-sensitive patterns are parsed as they are.
  - `-i` and smart case are parsed case-insensitively. For smart case that is a superset of
    whatever its rule decides, so the planner never has to repeat the rule.
  - The index is ASCII-folded, so either way the planner loses nothing for ASCII.
  - Non-ASCII case variants (`K` → `k`, `K` (Kelvin sign); `ſ` → `s`) arrive as small
    character classes and are expanded.
- Russ Cox's analysis runs over the HIR. Every node gets:
  - an exact set of strings
  - or, failing that, prefix and suffix sets
  - and a trigram AND/OR query that every match satisfies
  The rules only ever lose information soundly. Sets past a size limit are folded into the
  query and replaced by short or empty strings. Classes too big to expand mean "anything".
  `x*` and `x?` mean "anything, or nothing".
- If anything fails (a parse that `grep-regex` accepts but plain `regex-syntax` rejects, or
  an unexpected node), the plan is **all files**. The matcher still decides. A bad plan can
  only cost time, never a result.
- Execution intersects and unions posting lists. A trigram missing from the dictionary is an
  empty list, which is why `absent` reads no text at all.

### Segment file

One file, written to a temporary name, then flushed and renamed (invariant 4). Integers are
little-endian. A header and a trailer of section offsets carry magic numbers and a format
version. Every read is bounds-checked, so a damaged file is an error that leads to a rebuild,
never a wrong answer.

| section | contents | read |
|---|---|---|
| scope | roots (canonical), filter bits | at open |
| docs | per file: block, offset in block, stored length, path offset, root | at open |
| paths | paths relative to their root; UTF-8, or UTF-16 for the rare non-Unicode name | at open |
| errors | unreadable paths and their messages | at open |
| dictionary | sorted `(trigram, file count, postings offset)`, 16 bytes each | at open (2 MB on `dev`) |
| postings | per trigram: file ids in increasing order, as delta varints | per query trigram |
| blocks | per block: file offset, compressed and raw length | at open |
| store | zstd level 3 frames, each ~64 KiB of whole files packed in path order (a bigger file is one frame) | per candidate block |

- **zstd level 3**, as 0002 said, since the measurements back it. lz4 decompresses only 20%
  faster but is 48% bigger. Even a scan of the whole `dev` store (100 MB) decompresses in
  about 80 ms on one thread, or about 10–15 ms across the worker threads.
- **Positioned reads, not memory maps.** Mapping a file takes `unsafe`, which is only allowed
  in `mtseek-ntfs`. On `dev` the eagerly read sections total about 5 MB. Postings and blocks
  are read only when a query needs them. Revisit if a measurement says so.

### Lifecycle in M2

- **M2a:**
  - `mtseek --index PATTERN DIR…` searches through the content index for that scope.
  - It builds the index on first use, in the cache directory next to the name indexes.
  - Without `--index` nothing changes: the reference scanner answers.
  - The index is not yet caught up with file changes. Results reflect the build until
    `--reindex`, and `--index` documents that. Opting in is deliberate, so a search of `C:\`
    never quietly builds a multi-gigabyte index.
- **M2b:**
  - Catch up from the USN journal, as the name index does (0008): changed and new files go
    into a small delta segment, deleted ones into a tombstone list.
  - Merge segments when deltas grow.
  - Only then may the index answer without `--index` (invariant 3, no stale results).

## Consequences

- Invariant 2 rests on two arguments, each backed by a test:
  - the planner over-approximates: property tests generate random corpora and patterns and
    compare `mtseek-query` with `mtseek_walk::scan`
  - the store feeds the searcher the same bytes: those tests include binary files with early
    and late NULs, UTF-16 and UTF-8 BOMs, and NULs inside UTF-16 text
- `mtseek-cli`'s parity test also runs every case through `--index`.
- Writing the oracle test showed that ripgrep itself is not deterministic for a match shortly
  before a late NUL.
  - `grep-searcher`'s line buffer grows to fit a long line and is never shrunk.
  - In quit-on-NUL mode the searcher stops before searching a buffer fill that contains the
    NUL.
  - So a thread whose buffer grew on an earlier file never reports the match.
  `FileSearcher` rebuilds its searcher after any file big enough to grow the buffer. Scanner
  and index therefore always give the searched-alone answer. The parity test keeps excluding
  that one file, since `rg` still flips on it.
- The CLI depends on `mtseek-query` directly until the daemon exists (M3). After that, clients
  talk to `mtseekd` only, as the dependency rules say.
- The build holds all postings in memory: about 60 MB of ids on `dev`. Scopes far bigger than
  `dev` (the 43 GB `all` scope) need segmented builds, which arrive with M2b's segments.
- Patterns with no usable trigram scan the whole store. That is the honest worst case, and it
  is reported in the benchmark (`regex-nolit`).
