# 0013: The daemon and its pipe protocol

**Status:** Proposed, 2026-11-21. Makes the M3 part of [ARCHITECTURE](../ARCHITECTURE.md)
concrete. Builds on [0004](0004-service-security.md) (security) and
[0012](0012-index-coverage.md) (what is indexed).

## Context

Since M3a the CLI answers searches from coverage roots in direct mode. Every run pays for
the same things again:

| per search, `C:\Dev` | cost |
|---|---:|
| process start (the bench's spawn floor) | 16 ms |
| open the index | 3.6–4.7 ms |
| catch up, nothing relevant changed | 1.5–1.9 ms |
| catch up after one edit (re-list, read, write and reopen the delta) | 38–40 ms |

The agent server (M4) and the desktop app (M5) need the same engine without each embedding
it. The architecture already names the shape: `mtseekd`, a named pipe, and JSON-RPC 2.0 with
streamed results. This ADR settles what is expensive to change afterwards: the process
model, the transport and its security, the message format and versioning, and how clients
behave when there is no daemon.

## Decision

### 1. Process model

- **`mtseekd` is one binary with two modes:**
  - `mtseekd --console` runs in the foreground as the current user. It is the
    admin-free mode and the development mode.
  - Service mode (M3c) runs as LocalSystem, per 0004.
- **One daemon per user in console mode, one per machine in service mode.** A console daemon
  serves only its own user, so it needs no result filtering. The service filters results by
  the caller's access (0004, invariant 5).
- **What the daemon holds:**
  - the coverage list (0012), with every index open
  - the name indexes
  - one journal checkpoint per volume

  Indexes keep using positioned reads, not memory mapping, so all `unsafe` code stays in
  `mtseek-ntfs`. The OS file cache keeps them warm.

### 2. Transport and its security

- **A named pipe, local only** (`PIPE_REJECT_REMOTE_CLIENTS`).
  - Console mode: `\\.\pipe\mtseek-<user SID>`, with a DACL that admits only that SID.
  - Service mode: `\\.\pipe\mtseek`, with 0004's DACL.
  - A client checks the server process's identity before sending a query, so a process
    squatting on the name can't harvest queries. In console mode the server must run as the
    same SID; in service mode, as LocalSystem.
- **Pipe creation with a DACL and the identity checks need Win32 calls.** They go in
  `mtseek-ntfs/src/sys.rs`, the one place allowed `unsafe` (invariant 6). The crate's name
  no longer describes everything in it; renaming it is cosmetic and can wait.

### 3. Messages: newline-delimited JSON-RPC 2.0

- **Framing:** one JSON object per line, UTF-8, `\n`-terminated. It is easy to debug and
  easy for any client language. A binary frame can come later under a new protocol version
  if measurements demand it.
- **Methods:**
  - `initialize`: `{client, protocol: [min, max]}` returns
    `{server, protocol, capabilities}`.
  - `search`: query, roots, filter and mode, exactly what the CLI parses. The server sends
    `$/hit` notifications as files match (path, lines, binary offset), then the response
    `{summary, stats}`. Results are ripgrep's, decided by the same code paths as direct mode.
  - `names`: the same for `--names`.
  - `$/cancel {id}`: stops a search. The CLI sends it when its stdout pipe closes
    (`mtseek x | head`).
  - `status`: coverage, index sizes and journal positions.
  - `shutdown`.
- **Bytes stay exact.** ripgrep prints raw bytes, and parity is compared byte for byte.
  - A matched line is `{"text": "..."}` when it is valid UTF-8, and `{"b64": "..."}`
    otherwise.
  - A path is a JSON string when it is valid Unicode, and `{"wide": [u16, ...]}` otherwise.
    Windows allows unpaired surrogates in names.
- **Versioning:**
  - The protocol version is one integer, starting at 1. The client offers a range; the
    server picks the highest version both support, or answers with an error.
  - Adding optional fields doesn't change the version; both sides ignore fields they don't
    know.
  - Removing or changing the meaning of a field does.
- **The types live in a new crate, `mtseek-ipc`:** serde structs, framing, and version
  negotiation, with no index internals. Clients depend on it, never on `mtseek-index` (the
  dependency rule in ARCHITECTURE).

### 4. Clients without a daemon

- **The CLI tries the pipe first.**
  - It tries the per-user pipe, then the service pipe, with a connect timeout of 50 ms.
  - If neither answers, the version doesn't match, or the server's identity is wrong, it
    runs in direct mode (today's code), silently.
  - `--no-daemon` forces direct mode; `--stats` says which mode answered.
- **The CLI doesn't start a daemon.** In M3 it is started explicitly; the installer (M6)
  registers it.
- **Output is identical in both modes.** The parity, oracle and CLI tests run against a
  console daemon started on a temporary cache, as well as in direct mode.

### 5. Freshness: the dirty set

- **The daemon reads the journal continuously.** It polls every 250 ms, and also at the
  start of each search, so the "nothing changed" case costs one journal read.
- **Changes become a dirty set, not a delta per search.**
  - Dirty folders are re-listed with the walker, as in 0011. Changed files are read into
    an in-memory overlay.
  - A search uses the index minus the overlay's replaced files, plus the overlay.
  - The delta segment is written in the background after a quiet period of 2 s, or when the
    overlay holds 2,000 files. Writing it no longer adds to search time.
- **Invariant 3 stays exact:** a search first applies every journal record up to the
  position it read when it started.
- **Crash safety (invariant 4) doesn't change.** The overlay is only memory. After a crash,
  the next start catches up from the last written delta's checkpoint.

## Alternatives considered

- **gRPC or Cap'n Proto over the pipe.** Faster to parse, but heavier for clients written in
  other languages (the MCP server, editor plugins). They would also add a schema compiler
  to the build. Rejected for version 1; the version number leaves the door open.
- **Local TCP on 127.0.0.1.** Every local user and every browser tab can reach a TCP port,
  and access control is harder. Rejected; named pipes come with Windows ACLs.
- **The CLI starting the daemon automatically.** It would hide a background process the user
  never asked for, and the first search would pay its start-up cost. Deferred to the
  installer.
- **Keep catching up per search in the daemon (no overlay).** Simpler, but it keeps the
  38–40 ms edit latency, most of which is writing and reopening the delta.

## Consequences

- **New crates:** `mtseek-ipc` (types, framing, versioning) and `mtseekd` (the binary). New
  dependencies: `serde`, `serde_json`, `tracing`, all licence-checked by `cargo deny`.
- **Expected per-search cost** with a running daemon: process start, connecting to the pipe,
  then the search itself. The index is already open and current. The benchmark gains
  `mtseek` (daemon) next to `mtseek-index` (direct) to measure it.
- **Edit latency** should drop from 38–40 ms to the cost of re-listing one folder and
  reading one file, about 5 ms. To be measured.
- **The parity suite runs twice** (direct and daemon), so CI time grows.
- **This ADR changes nothing in 0012** (what is indexed) or 0004 (how the service filters
  results); it only provides the process they run in.
