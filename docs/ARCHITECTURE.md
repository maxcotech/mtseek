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
| `mtseek-walk` | M0 ✅ | The **reference scanner**: walks a scope and searches every file with ripgrep-identical semantics. Used as the test oracle, as the fallback for non-NTFS volumes and index-hostile patterns, and to search the dirty set. Its `list` (the scope's files and directories) and `FileSearcher` (one file's search) are what the content index builds from and verifies with. |
| `mtseek-cli` | M0 ✅ | The `mtseek` binary: ripgrep's flags and output format (ADR 0005). Sends the search to the user's `mtseekd`, else to the service (ADR 0013, 0014), else runs `mtseek-engine` itself ("direct mode"); prints the same either way. A search the service declines runs in direct mode. |
| `mtseek-testkit` | M0 ✅ | Deterministic synthetic corpora full of edge cases. |
| `xtask` | M0 ✅ | `cargo xtask ci / bench / corpus / fmt`. |
| `mtseek-ntfs` | M1 ✅ | Raw MFT record reading with every hard link (ADR 0009) and MFT enumeration as its fallback, both needing admin; unprivileged USN journal reads through a directory handle, directory listings with file reference numbers, long paths. Safe parsers for every buffer format, property-tested against random and malformed input. Also the daemon's named pipe (`pipe`, ADR 0013): a DACL for the user's SID (or the service's), remote clients refused, the server's identity checked by the client. And service mode (ADR 0014): security descriptors, callers' tokens and `AccessCheck` (`security`, every descriptor checked for structure in safe code first), the SCM and the machine data folder (`service`). **The only crate allowed `unsafe`**, confined to `src/sys.rs`. |
| `mtseek-names` | M1 ✅ | File-name index: flat name arenas + parent links, parallel SIMD substring scan (ADR 0007). Built from the MFT or by a parallel directory walk; saved as a snapshot and caught up from the USN journal by re-listing changed directories (ADR 0008). |
| `mtseek-index` | M2 ✅ | Segment file (ADR 0010; v4 since ADR 0014): trigram dictionary, delta-varint postings, document table, zstd content store of raw file bytes cut where ripgrep stops reading, and a table of security descriptors that files, folders and control files refer to. Secret-looking files are listed but never stored; encrypted files and cloud placeholders are never read. Built from a scope's file list; opened with every section bounds-checked. Kept current (ADR 0011): file, folder and control-file reference numbers plus journal checkpoints in the segment, catch-up by re-listing dirty folders into a delta segment over the immutable base, rebuild when the delta grows past 1/8 of the base. Coverage (ADR 0012): each file, folder and error records which of the four ignore/hidden filter combinations would list it, so one index answers its subfolders and every narrower filter; files cut at a NUL are flagged for binary-mode searches. |
| `mtseek-query` | M2 ✅ | Regex → trigram plan (`regex-syntax` HIR, Russ Cox's analysis), posting-list execution limited to the searched folder and filter, verification of candidates with `mtseek-walk`'s `FileSearcher` on stored bytes. Before planning, the caller's visible set (ADR 0014): folders it may list and files it may read, by `Viewer`; the rest is reported as `rg` reports it and never reaches planning, verification or the summary. |
| `mtseek-engine` | M3b ✅ | What answers a search, for both hosts: coverage roots and their registry (ADR 0012), opening and catching up content indexes, scanning what no index covers, name indexes. `Engine::direct` serves one CLI run and writes every change first; `Engine::resident` (the daemon) keeps indexes open, catches them up between searches and keeps deltas in memory until they are quiet. Requests can carry the caller's working directory; results are spelled onto the roots as typed. Every search runs for a `Caller` (this process, or a daemon's client by its token); `Engine::serve` answers only from indexes, exactly, or declines before reporting anything (the service, ADR 0014). |
| `mtseek-ipc` | M3b ✅ | The daemon protocol (ADR 0013): newline-delimited JSON-RPC 2.0, typed messages, exact bytes (lines as UTF-8 text or base64, paths as strings or UTF-16), version negotiation, framing with a size limit, and the pipe client. No index internals. |
| `mtseek-daemon` | M3b ✅ | `mtseekd`: one resident engine behind a local named pipe. `--console` runs it per user without admin; a thread per connection, searches one at a time, `$/hit` streaming, cancellation, a poller that catches indexes up every 250 ms and writes deltas after 2 s of quiet. M3c (ADR 0014): `--install`/`--uninstall`/`--service` run it as the LocalSystem service for every interactive user, from `%ProgramData%\mtseek`, each client's token deciding what it sees; it declines what it can't answer exactly, logs no paths or queries, and stops only for an elevated administrator. The scheduler is later. |
| `mtseek-mcp` | M4 | MCP server for AI agents. |
| `mtseek-app` | M5 | GPUI desktop app (ADR 0003). |

### Dependency rules

- `mtseek-core` depends on nothing internal. Everything may depend on it.
- Clients (`cli`, `mcp`, `app`) depend on `mtseek-ipc`, **never** on index internals.
  The engine can then change its storage without touching a client. The one exception is
  `mtseek-cli`'s direct mode, which runs `mtseek-engine` in-process when no daemon answers;
  it still never touches `mtseek-index` or `mtseek-query` itself.
- `mtseek-engine` is the only crate that combines the index, the query layer, the names
  index and the scanner. `mtseek-daemon` and `mtseek-cli` host it; they add no search logic.
- `mtseek-query` verifies with `mtseek-walk`'s `FileSearcher`, never its own matcher setup,
  so the index path and the scanner can't drift apart.
- `mtseek-walk` must stay ripgrep-identical. It is what every index result is checked against.
- `mtseek-names` and `mtseek-index` use `mtseek-ntfs` (Windows only) for listings, file
  reference numbers, the journal and the MFT. Nothing else talks to NTFS directly.

## Invariants

These are properties the code must keep. Each one has a test that enforces it.

1. **Parity.** For any corpus, query and filter, mtseek reports exactly what `rg` reports.
   Enforced by `crates/mtseek-cli/tests/parity.rs` (every case in direct mode and through a
   daemon) and by the benchmark's parity check.
2. **Index = oracle.** (M2) For any corpus and query, the index path returns exactly what the
   reference scanner returns: same files, lines and binary warnings. Enforced by
   `crates/mtseek-query/tests/oracle.rs` (random corpora with BOMs, NULs and non-ASCII case
   variants, random regexes, every filter level, plus the edge-case corpus) and by the parity
   test, which runs every case through `--index` too. The planner may only over-approximate;
   anything it can't analyse plans "all files". Since ADR 0012 the oracle also searches
   subfolders of the index with every filter it covers, and
   `subfolder_searches_through_a_parent_index_match_ripgrep` checks the same against `rg`.
   Both rest on the subtree rule, enforced by `crates/mtseek-walk/tests/subtree.rs`.
3. **No stale results.** A file saved before a query starts is searched as saved.
   For the content index (M2b, ADR 0011): every search catches the index up from the change
   journal first. Enforced by `crates/mtseek-index/tests/live.rs`: random sequences of
   changes, after each of which the caught-up index must equal a fresh build. The CLI test
   `tests/content_index.rs` checks the same end to end. Since ADR 0012 `.git/info/exclude`
   and the ignore files above an indexed folder are tracked too (an edit above the folder
   rebuilds). Known gaps: git's global excludes file, a `.git/info` folder created after
   the index was built, and repositories whose `.git` is a file (worktrees, submodules)
   need `--reindex`. Permission changes count (ADR 0014): catch-up records the new security
   descriptor of every file and folder the journal says changed, and the next search honours
   it; `live.rs` changes DACLs and their inheritance and checks the descriptors against a
   fresh build. The daemon (M3b, ADR 0013) catches its open indexes up every 250 ms
   and again before each search, and keeps the new delta in memory until changes have been
   quiet for 2 s. `live.rs` also keeps deltas in memory, saves them, and drops unsaved ones
   (a daemon that stopped), checking against a fresh build after each step.
4. **Crash-safe index.** Killing `mtseekd` at any instant leaves an index that opens and is correct.
   Segments are written to a temporary file, flushed, then renamed over the old one. A delta
   the daemon holds in memory is only memory: after a crash the last written delta and its
   checkpoint are caught up again.
5. **No privilege leak.** (M3c, ADR 0014) A caller sees exactly what `rg` run by that caller
   would show: names in folders it may list (from the search root down), lines from files it
   may read, and `Access is denied` for the rest. Decided by `AccessCheck` of the descriptors
   the index recorded against the caller's token, before planning, so no count or line of an
   invisible file reaches the output. Enforced by `crates/mtseek-cli/tests/access.rs` (direct
   and daemon, after taking access away from the test's own account), the service tests in
   `mtseek-daemon`, and the two-account test (`tests/service.rs`, elevated CI).
6. **Only `mtseek-ntfs` contains `unsafe`**, and only in `src/sys.rs` (NTFS, and the pipe and token calls of the daemon's transport). Enforced by `unsafe_code = "forbid"` in workspace lints and `deny` + one scoped `allow` in that crate.
7. **Name search is complete.** `mtseek --names ""` lists every file `rg --files -uu` lists (plus directories). Enforced by `crates/mtseek-cli/tests/names.rs`. Whole-drive indexes read raw MFT records so that every hard link is listed (ADR 0009); the admin-only tests in CI enforce it.
8. **A snapshot is only a cache.** A caught-up snapshot gives the same answers as a fresh walk, and a missing or corrupt one costs time, never correctness. Enforced by `catch_up_matches_a_fresh_walk` (`mtseek-names`), the snapshot corruption property test, and `later_runs_reuse_the_saved_index_and_see_changes` (CLI).
9. **The daemon changes nothing but speed.** (M3b) Through `mtseekd`, `mtseek` prints the same bytes on stdout and stderr and exits with the same code as in direct mode: non-UTF-8 lines, non-Unicode names, relative roots, unreadable roots and bad patterns included. Enforced by `crates/mtseek-cli/tests/daemon.rs`, and by `parity.rs`, `content_index.rs` and `names.rs` running through a daemon too.

## Cross-cutting concerns

- **Errors.** Library crates return `mtseek_core::Error`, and binaries add context with `anyhow`.
  Unreadable files are reported and skipped, never fatal (ripgrep's behaviour, exit code 2).
- **Logging.** The CLI prints nothing but results to stdout. `mtseekd --console` logs one
  line per request to stderr. The service logs one line per request (method, caller SID,
  outcome, duration) to `%ProgramData%\mtseek\logs`, two files of at most 8 MiB, and never a
  path or a query; the SCM's own events record starts, stops and crashes.
- **Performance work** starts with a benchmark number and ends with one: `cargo xtask bench`.
  A speed claim without a run in `bench/results/` doesn't count.
- **Windows specifics.** Long paths (`\\?\`), reparse points (not followed by default),
  hard links (the name index lists each link, like `rg --files`), UTF-16 files (BOM sniffing),
  USN journal wrap or recreation (full rebuild).
- **Security.** The index holds file contents, so it is as sensitive as the disk.
  The service's index directory is ACL'd to SYSTEM + Administrators (and recreated if someone
  else created it first), and secret-looking files are never stored in any index
  (ADR 0004, 0012 §4, 0014). Requests from other users are untrusted input: the protocol
  parser and the descriptor checker are property-tested on damaged input.

## Milestones

| | Milestone | Done when |
|---|---|---|
| **M0** | Foundation | Workspace, CI, xtask, ADRs, benchmark harness with ripgrep baseline, parity test passing. |
| **M1** | Name engine | `mtseek --names` from MFT or parallel walk; index persisted between runs and caught up from the USN journal without admin (ADR 0008). Whole-drive indexes list every hard link from raw MFT records (M1c, ADR 0009). |
| M2 | Content engine | Trigram index + content store; `dev` scope queries under 100 ms with parity. M2a (ADR 0010): index built with `--index`, parity in every benchmark cell. M2b (ADR 0011): caught up from the USN journal on every search, used automatically once a scope has one. 12 of 16 `dev` cells under 100 ms; the rest are output volume, the no-literal regex and noise (`bench/TARGETS.md`). |
| M3 | Daemon | Service, IPC, ACL filtering, dirty set, admin-free fallback mode. M3a: coverage roots (ADR 0012) in the CLI's direct mode. M3b (ADR 0013): `mtseekd --console`, the pipe protocol, one engine for both modes, deltas in memory until quiet. M3c (ADR 0014): the LocalSystem service, each caller's access checked against recorded descriptors, declines for what isn't exact. Left: names through the service, the global excludes file applied per caller, serving the walker path. |
| M4 | Agents | MCP server, `--json`, docs/mcp.md; Claude Code uses it end to end. |
| M5 | Desktop app | GPUI palette, preview, settings, onboarding, tray. Preceded by a GPUI spike. |
| M6 | Ship | Signed MSI, winget, auto-update, docs site. |
