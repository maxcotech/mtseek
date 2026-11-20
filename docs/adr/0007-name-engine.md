# 0007: Name engine: flat arenas, parent links, two sources

**Status:** Accepted, 2026-11-19. Partly superseded by [0008](0008-persisted-name-index.md):
ids, hard links and how journal changes are applied.

## Context

File-name search ("where is `PaystackClient.cs`?") has to cover every file on a volume,
1–2M entries on a dev machine, and answer per keystroke. It also has to absorb renames and
moves of whole directory trees without rewriting thousands of paths.

Measured on `C:\Dev` (1.21M entries, the reference laptop): ripgrep needs 4.3–6.6 s just to
*list* the tree (`rg --files -uu`), before any filtering.

## Decision

**Index layout (`mtseek-names`).**
- Each entry stores only `(id, parent id, name)`. Names live in two flat byte arenas,
  original case and case-folded, each name prefixed by a NUL separator.
- A query is one `memchr::memmem` SIMD scan over the arena, split across threads at
  separators, with hits mapped to entries by binary search.
- Paths are rebuilt from parent links, and only for results.
- Renames and moves are a single entry update. Removed entries are tombstoned and
  `compact()` reclaims them.

**Two sources, one index.**
- *NTFS MFT* (`mtseek-ntfs`, needs admin): `FSCTL_ENUM_USN_DATA` lists every entry without
  touching directories. Ids are NTFS file reference numbers, so USN journal records
  (M1b/M3) apply directly as `Change`s.
- *Parallel directory walk* (portable, no admin): workers share a directory stack and number
  entries from an atomic counter. It covers the same set: hidden and ignored files, no link
  following.

**Query semantics.** Space-separated terms must all appear in the name. Smart case by
default (Everything-style; deliberately not ripgrep's case-sensitive default, because names
aren't code). Results are absolute paths, sorted. Directories match too.

## Consequences

- Measured: a query over 1.21M entries takes **1.5 ms**. Building by parallel walk takes
  3.9–6.5 s (on par with `rg --files`). That's 152–174 MiB in memory, mostly
  `HashMap` and arena slack. Compaction and a tighter id map are later work.
- `mtseek --names` builds the index on every run until the index persists between runs
  (M1b: snapshot + USN catch-up) or lives in the daemon (M3). The engine is instant; the
  CLI isn't yet.
- Completeness is tested, not assumed. `tests/names.rs` requires every file `rg --files -uu`
  lists to appear, and every extra to be a directory. On the real `C:\Dev`: 1,057,070/1,057,070
  files present, 155,388 extras, all directories.
- MFT enumeration can't be exercised without admin. Its test skips locally and is required
  in CI (`MTSEEK_REQUIRE_ADMIN=1`, elevated runners).
- Names with unpaired UTF-16 surrogates are shown with U+FFFD (see `UsnRecord::name`).
