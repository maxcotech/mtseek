# Architecture

This is the map. Read it before changing code; update it when the map changes.
Decisions and their reasons live in [`adr/`](adr/).

## The promise

Type anything and see every match on the machine, in file names and file contents,
in **under 100 ms, CLI process start included**. Results are always current and nothing
rescans in the background. The bar comes from measurements, not hopes. See
[`bench/TARGETS.md`](../bench/TARGETS.md). On this project's reference laptop ripgrep takes
**~570 ms** on a 579 MB dev tree and **~3 minutes** on the full 43.6 GB tree. Every query
costs about the same because **opening files is the cost**: 38–175 µs per file, mostly NTFS
and the Windows Defender filter driver.

So the whole design follows one rule: **at query time, open as few files as possible. Ideally none.**

## Bird's-eye view

```
 ┌──────────────┐  ┌──────────────┐  ┌──────────────┐
 │ mtseek-app   │  │ mtseek (CLI) │  │ mtseek-mcp   │   clients, run as the user
 │ GPUI         │  │ rg-compatible│  │ MCP / stdio  │
 └──────┬───────┘  └──────┬───────┘  └──────┬───────┘
        └────── named pipe · JSON-RPC 2.0, streaming results ─┘
                           │
 ┌─────────────────────────▼───────────────────────────┐
 │ mtseekd: Windows service (or `--console` for dev)   │
 │  ├─ volume watchers   NTFS MFT + USN journal        │
 │  │                    fallback: walker + RDCW       │
 │  ├─ name index        in memory, SIMD linear scan   │
 │  ├─ content index     trigram segments + compressed │
 │  │                    content store, mmap'd         │
 │  ├─ planner           regex → trigram AND/OR plan   │
 │  ├─ verifier          ripgrep's matcher on candidates│
 │  ├─ dirty set         files changed since last commit│
 │  └─ scheduler         low I/O priority, battery-aware│
 └─────────────────────────────────────────────────────┘
```

A query runs like this:

1. The planner turns the pattern into a trigram query, e.g. `Paystack` → `Pay ∧ ays ∧ yst ∧ sta ∧ tac ∧ ack`.
2. The content index intersects posting lists. The result is a small set of candidate documents.
3. The verifier runs the real matcher (ripgrep's `grep-searcher`) over each candidate's text,
   read from the **content store** (ADR 0002), not by opening the file.
4. Files in the **dirty set** (changed after the last index commit) are searched directly by the
   reference scanner, so results are never stale.
5. Results stream back over the pipe as they are found.

A pattern with no usable trigram (`\b\d{3}-\d{3}-\d{4}\b`) runs the verifier over the content
store without narrowing. That is still far cheaper than opening files, but it is the honest worst case.

## Code map

Crates land with the milestone that needs them. There are no empty placeholder crates.

| Crate | Status | Responsibility |
|---|---|---|
| `mtseek-core` | M0 ✅ | Shared vocabulary: `Query`, `Scope`, `Filter`, `Error`. No I/O. |
| `mtseek-walk` | M0 ✅ | The **reference scanner**: walks a scope and searches every file with ripgrep-identical semantics. Used as the test oracle, as the fallback for non-NTFS volumes and index-hostile patterns, and to search the dirty set. Its `list_files` (the scope's file set) and `FileSearcher` (one file's search) are what the content index builds from and verifies with. |
| `mtseek-cli` | M0 ✅ | The `mtseek` binary. ripgrep's flags and output format (ADR 0005). Talks to `mtseekd` from M3 on; until then it scans, or searches the content index with `--index` (M2a). |
| `mtseek-testkit` | M0 ✅ | Deterministic synthetic corpora full of edge cases. |
| `xtask` | M0 ✅ | `cargo xtask ci / bench / corpus / fmt`. |
| `mtseek-ntfs` | M1 ✅ | Raw MFT record reading with every hard link (ADR 0009) and MFT enumeration as its fallback, both needing admin; unprivileged USN journal reads through a directory handle, directory listings with file reference numbers, long paths. Safe parsers for every buffer format, property-tested against random and malformed input. **The only crate allowed `unsafe`**, confined to `src/sys.rs`. |
| `mtseek-names` | M1 ✅ | File-name index: flat name arenas + parent links, parallel SIMD substring scan (ADR 0007). Built from the MFT or by a parallel directory walk; saved as a snapshot and caught up from the USN journal by re-listing changed directories (ADR 0008). |
| `mtseek-index` | M2a ✅ | Segment file (ADR 0010): trigram dictionary, delta-varint postings, document table, zstd content store of raw file bytes cut where ripgrep stops reading. Built from a scope's file list; opened with every section bounds-checked. Manifest, delta segments and merges arrive with M2b. |
| `mtseek-query` | M2a ✅ | Regex → trigram plan (`regex-syntax` HIR, Russ Cox's analysis), posting-list execution, verification of candidates with `mtseek-walk`'s `FileSearcher` on stored bytes. |
| `mtseek-ipc` | M3 | JSON-RPC protocol types + versioning, shared by every client. |
| `mtseekd` | M3 | Service host, scheduler, IPC server, ACL enforcement. |
| `mtseek-mcp` | M4 | MCP server for AI agents. |
| `mtseek-app` | M5 | GPUI desktop app (ADR 0003). |

### Dependency rules

- `mtseek-core` depends on nothing internal. Everything may depend on it.
- Clients (`cli`, `mcp`, `app`) depend on `mtseek-ipc`, **never** on index internals.
  The engine can then change its storage without touching a client. Until the daemon exists
  (M3), `mtseek-cli` calls `mtseek-query` and opens segments through `mtseek-index` directly.
- `mtseek-query` verifies with `mtseek-walk`'s `FileSearcher`, never its own matcher setup,
  so the index path and the scanner can't drift apart.
- `mtseek-walk` must stay ripgrep-identical. It is what every index result is checked against.
- `mtseek-names` uses `mtseek-ntfs` (Windows only) for listings, the journal and the MFT. Nothing
  else talks to NTFS directly.

## Invariants

These are properties the code must keep. Each one has a test that enforces it.

1. **Parity.** For any corpus, query and filter, mtseek reports exactly what `rg` reports.
   Enforced by `crates/mtseek-cli/tests/parity.rs` and by the benchmark's parity check.
2. **Index = oracle.** (M2) For any corpus and query, the index path returns exactly what the
   reference scanner returns: same files, lines and binary warnings. Enforced by
   `crates/mtseek-query/tests/oracle.rs` (random corpora with BOMs, NULs and non-ASCII case
   variants, random regexes, every filter level, plus the edge-case corpus) and by the parity
   test, which runs every case through `--index` too. The planner may only over-approximate;
   anything it can't analyse plans "all files".
3. **No stale results.** A file saved before a query starts is searched as saved.
   The dirty set guarantees it.
4. **Crash-safe index.** Killing `mtseekd` at any instant leaves an index that opens and is correct.
   Commits are write-new-then-rename of a versioned manifest, with fsync.
5. **No privilege leak.** (M3) A user never sees a path or a line from a file their own
   account cannot open (ADR 0004).
6. **Only `mtseek-ntfs` contains `unsafe`**, and only in `src/sys.rs`. Enforced by `unsafe_code = "forbid"` in workspace lints and `deny` + one scoped `allow` in that crate.
7. **Name search is complete.** `mtseek --names ""` lists every file `rg --files -uu` lists (plus directories). Enforced by `crates/mtseek-cli/tests/names.rs`. Whole-drive indexes read raw MFT records so that every hard link is listed (ADR 0009); the admin-only tests in CI enforce it.
8. **A snapshot is only a cache.** A caught-up snapshot gives the same answers as a fresh walk, and a missing or corrupt one costs time, never correctness. Enforced by `catch_up_matches_a_fresh_walk` (`mtseek-names`), the snapshot corruption property test, and `later_runs_reuse_the_saved_index_and_see_changes` (CLI).

## Cross-cutting concerns

- **Errors.** Library crates return `mtseek_core::Error`, and binaries add context with `anyhow`.
  Unreadable files are reported and skipped, never fatal (ripgrep's behaviour, exit code 2).
- **Logging.** `tracing` from M3 on. The CLI prints nothing but results to stdout.
- **Performance work** starts with a benchmark number and ends with one: `cargo xtask bench`.
  A speed claim without a run in `bench/results/` doesn't count.
- **Windows specifics.** Long paths (`\\?\`), reparse points (not followed by default),
  hard links (the name index lists each link, like `rg --files`), UTF-16 files (BOM sniffing),
  USN journal wrap or recreation (full rebuild).
- **Security.** The index holds file contents, so it is as sensitive as the disk.
  The index directory is ACL'd to SYSTEM + Administrators, and secret-looking files are excluded
  by default (ADR 0004).

## Milestones

| | Milestone | Done when |
|---|---|---|
| **M0** | Foundation | Workspace, CI, xtask, ADRs, benchmark harness with ripgrep baseline, parity test passing. |
| **M1** | Name engine | `mtseek --names` from MFT or parallel walk; index persisted between runs and caught up from the USN journal without admin (ADR 0008). Whole-drive indexes list every hard link from raw MFT records (M1c, ADR 0009). |
| M2 | Content engine | Trigram index + content store; `dev` scope queries under 100 ms with parity. M2a (ADR 0010): index built on first `--index` search, parity in every benchmark cell. M2b: caught up from the USN journal, then on by default. |
| M3 | Daemon | Service, IPC, ACL filtering, dirty set, admin-free fallback mode. |
| M4 | Agents | MCP server, `--json`, docs/mcp.md; Claude Code uses it end to end. |
| M5 | Desktop app | GPUI palette, preview, settings, onboarding, tray. Preceded by a GPUI spike. |
| M6 | Ship | Signed MSI, winget, auto-update, docs site. |
