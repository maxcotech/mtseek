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
| `mtseek-walk` | M0 ✅ | The **reference scanner**: walks a scope and searches every file with ripgrep-identical semantics. Used as the test oracle, as the fallback for non-NTFS volumes and index-hostile patterns, and to search the dirty set. |
| `mtseek-cli` | M0 ✅ | The `mtseek` binary. ripgrep's flags and output format (ADR 0005). Talks to `mtseekd` from M3 on; for now it scans. |
| `mtseek-testkit` | M0 ✅ | Deterministic synthetic corpora full of edge cases. |
| `xtask` | M0 ✅ | `cargo xtask ci / bench / corpus / fmt`. |
| `mtseek-ntfs` | M1 | MFT enumeration (`FSCTL_ENUM_USN_DATA`) and USN journal tailing. **The only crate allowed `unsafe`** (Win32 FFI). |
| `mtseek-names` | M1 | File-name index: name arena + parent links, parallel SIMD substring scan. |
| `mtseek-index` | M2 | Segment format: trigram dictionary, delta-encoded postings, document table, zstd content store, manifest, merges. |
| `mtseek-query` | M2 | Regex → trigram plan (`regex-syntax` HIR), execution, verification, ranking. |
| `mtseek-ipc` | M3 | JSON-RPC protocol types + versioning, shared by every client. |
| `mtseekd` | M3 | Service host, scheduler, IPC server, ACL enforcement. |
| `mtseek-mcp` | M4 | MCP server for AI agents. |
| `mtseek-app` | M5 | GPUI desktop app (ADR 0003). |

### Dependency rules

- `mtseek-core` depends on nothing internal. Everything may depend on it.
- Clients (`cli`, `mcp`, `app`) depend on `mtseek-ipc`, **never** on index internals.
  The engine can then change its storage without touching a client.
- `mtseek-walk` must stay ripgrep-identical. It is what every index result is checked against.

## Invariants

These are properties the code must keep. Each one has a test that enforces it.

1. **Parity.** For any corpus, query and filter, mtseek reports exactly what `rg` reports.
   Enforced by `crates/mtseek-cli/tests/parity.rs` and by the benchmark's parity check.
2. **Index = oracle.** (M2) For any corpus and query, the index path returns exactly what the
   reference scanner returns. Enforced by property tests that generate random corpora and patterns.
3. **No stale results.** A file saved before a query starts is searched as saved.
   The dirty set guarantees it.
4. **Crash-safe index.** Killing `mtseekd` at any instant leaves an index that opens and is correct.
   Commits are write-new-then-rename of a versioned manifest, with fsync.
5. **No privilege leak.** (M3) A user never sees a path or a line from a file their own
   account cannot open (ADR 0004).
6. **Only `mtseek-ntfs` contains `unsafe`.** Enforced by `unsafe_code = "forbid"` in workspace lints.

## Cross-cutting concerns

- **Errors.** Library crates return `mtseek_core::Error`, and binaries add context with `anyhow`.
  Unreadable files are reported and skipped, never fatal (ripgrep's behaviour, exit code 2).
- **Logging.** `tracing` from M3 on. The CLI prints nothing but results to stdout.
- **Performance work** starts with a benchmark number and ends with one: `cargo xtask bench`.
  A speed claim without a run in `bench/results/` doesn't count.
- **Windows specifics.** Long paths (`\\?\`), reparse points (not followed by default),
  hard links (deduplicated by file ID), UTF-16 files (BOM sniffing), USN journal wrap (full rescan).
- **Security.** The index holds file contents, so it is as sensitive as the disk.
  The index directory is ACL'd to SYSTEM + Administrators, and secret-looking files are excluded
  by default (ADR 0004).

## Milestones

| | Milestone | Done when |
|---|---|---|
| **M0** | Foundation | Workspace, CI, xtask, ADRs, benchmark harness with ripgrep baseline, parity test passing. |
| M1 | Name engine | `mtseek --files` style name search from MFT + USN, live, beating Everything/fd. |
| M2 | Content engine | Trigram index + content store; `dev` scope queries under 100 ms with parity. |
| M3 | Daemon | Service, IPC, ACL filtering, dirty set, admin-free fallback mode. |
| M4 | Agents | MCP server, `--json`, docs/mcp.md; Claude Code uses it end to end. |
| M5 | Desktop app | GPUI palette, preview, settings, onboarding, tray. Preceded by a GPUI spike. |
| M6 | Ship | Signed MSI, winget, auto-update, docs site. |
