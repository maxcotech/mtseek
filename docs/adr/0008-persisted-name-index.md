# 0008: Persisted name index, caught up from the journal without admin

**Status:** Accepted, 2026-11-20. Its known hard-link gap is closed by
[0009](0009-raw-mft-records.md). Supersedes the "USN records apply directly as `Change`s"
part of [0007](0007-name-engine.md); the rest of 0007 stands.

## Context

After M1, `mtseek --names` rebuilt its index on every run: 3.9–6.5 s on `C:\Dev` by
walking, or about 5.6 s from the MFT with admin. The search itself took 1.5 ms. The daemon
(M3) will keep the index live in memory, but the CLI is useful before then and will stay
the fallback when no daemon runs.

The plan in 0007 was to apply USN journal records as index changes. That needs record
*names*, and names need admin: reading the journal through a volume handle (`\\.\C:`)
needs admin. A probe on this machine, as a normal user, found:

- A handle to a **directory** of the volume (`C:\`, `FILE_READ_ATTRIBUTES`) is enough for
  `FSCTL_QUERY_USN_JOURNAL` and `FSCTL_READ_UNPRIVILEGED_USN_JOURNAL` (Windows 10 1709+).
- Unprivileged records have **empty names**, but keep the file reference, the parent
  directory's reference and the reason flags.
- `FSCTL_ENUM_USN_DATA` (MFT enumeration) still needs admin.

## Decision

**Persist the index** as a snapshot with the journal position it reflects:
`%LOCALAPPDATA%\mtseek\cache` (or `MTSEEK_CACHE_DIR`). A snapshot is a whole-drive MFT
index (`names-C.mft.idx`) or one walked root (`names-walk-<hash>.idx`). Writes are
temp-file-then-rename. Loading checks every offset and rejects anything malformed, and
the caller then rebuilds. Only live entries are written, so a saved index is compact.

**Catch up by re-listing, not by applying records.** Read the unprivileged journal from
the saved position, and collect the parent directories of records whose reasons can change
names (create, delete, rename, hard link, reparse point). Data writes, most of the journal,
are ignored. List each of those directories again and replace its children in the index.
Walk any directory that is new. One path for admin and non-admin. A snapshot built with
admin is kept current by non-admin runs.

To make journal records match index entries, **ids are NTFS file reference numbers in
both sources**. The walker lists directories with
`GetFileInformationByHandleEx(FileIdBothDirectoryInfo)`, the same kernel call as
`FindFirstFile`, which returns each entry's reference number at no extra cost.

Details that make re-listing correct:
- **Stale paths.** Paths come from the not-yet-updated index. A listing counts only if
  the directory found at the path has the expected reference number. Otherwise it is
  retried after the next round that made progress, which covers renamed ancestors and
  name swaps.
- **Subtree changes.** A directory missing from every new listing is dropped with its
  subtree, in one orphan-pruning pass. A directory that moved between two listed parents
  keeps its subtree.
- **Hard links.** Files are not keyed by id; only directories are. A file with several
  hard links appears once per directory, like `rg --files`.
- **Saving.** A run saves only if a listing actually differed. Otherwise the run's own
  save, which touches the cache directory, would force a save on every following run.
- **Lost history.** A changed journal id, or a position older than the journal keeps,
  means a full rebuild.

## Consequences

Measured on the reference laptop, `C:\Dev`. Process start alone is 65 ms, the same as `rg`.

| Run | Before (M1) | Now |
|---|---|---|
| Walk-built index, repeat run | 3.9–6.5 s | **~200 ms** wall (load 100 ms, search 1.5 ms) |
| Whole-drive MFT index (2.43M entries), repeat run as a normal user | 5.6 s, admin only | **~350 ms** wall (load 220–270 ms), ~700 ms when it re-saves |
| First run (walk, nothing saved) | 3.9–6.5 s | 4.6 s + 0.3 s save |

- Snapshots are 81 MB for 1.2M entries. Loading is a straight parse with bounds checks.
  A memory-mapped format could cut load time further; the daemon (M3) removes it entirely.
- **Known gap: hard links in MFT-built indexes.** `FSCTL_ENUM_USN_DATA` reports one name
  per file, so the other links of a hard-linked file are missing. On `C:\Dev` that is
  3,877 files, all Gradle/CMake build intermediates. The walk source has no gap
  (1,059,893/1,059,893 files `rg --files -uu` lists). Fixing it means reading `$FILE_NAME`
  attributes from raw MFT records, which is planned as M1c.
- A walked snapshot only serves the exact root it was built for. Serving a subdirectory
  from an ancestor's snapshot is easy later work.
- The snapshot holds every file name on the drive, in the user's own profile. An MFT
  snapshot is written by an elevated run of the same user. The service (ADR 0004) will
  keep its index elsewhere, ACL'd.
