# 0015: Agents: `--json`, the flags agents use, and an MCP server

**Status:** Accepted, 2026-11-22. First decision of M4; M4a is built (see
[As built](#as-built-m4a)). Builds on
[0005](0005-ripgrep-compatible-contract.md) (ripgrep is the contract),
[0013](0013-daemon-and-ipc.md) (the pipe protocol) and [0014](0014-service-mode.md) (what a
caller may see). Amends one dependency rule in [ARCHITECTURE](../ARCHITECTURE.md); see
[Consequences](#consequences).

## Context

AI coding agents search code constantly, and today they do it by running `rg` or a built-in
tool that wraps it. Each call pays ripgrep's full price: about 0.6 s on a 579 MB tree, minutes
on a whole drive. mtseek answers the same searches from an index in tens of milliseconds
(`bench/TARGETS.md`), but an agent can only use it by shelling out to `mtseek`, which then
prints plain text that the agent has to parse.

What an agent's search tool needs, taking Claude Code's built-in `Grep` tool as the reference
because agents are trained on it:

| needed | mtseek today |
|---|---|
| pattern, path, `-i`, `-n`, files-with-matches | yes |
| context lines (`-A`, `-B`, `-C`) | no |
| file globs (`-g`) and file types (`-t`) | no |
| counts per file (`-c`) | no |
| a limit on how much comes back | no (`| head` only) |
| machine-readable output (`rg --json`) | no |
| multiline patterns (`-U`) | no |

The MCP specification has two versions in use. 2025-11-25 opens each connection with an
`initialize` handshake. 2026-07-28 (released in July) removes it: every request carries its
protocol version in `_meta`, servers must answer `server/discover`, every result carries a
`resultType`, and `tools/list` results carry `ttlMs` and `cacheScope`. Anthropic says Claude
products will support it "soon", without versions, so clients in use today almost certainly
speak 2025-11-25. The official Rust SDK (`rmcp`) supports 2026-07-28 only in beta.

## Decision

### 1. Two steps

- **M4a: the CLI.** The flags in §2 and `--json` (§3), with ripgrep parity, in direct mode and
  through the daemon. Agents that shell out benefit at once.
- **M4b: the MCP server** (§5–§8), built on what M4a added, with `docs/mcp.md`.

### 2. The flags agents use (M4a)

- **New flags, with ripgrep's meaning:** `-A`/`-B`/`-C` (context), `-g`/`--glob` and
  `--iglob`, `-t`/`-T` (file types, with ripgrep's built-in definitions), `-c`/`--count`,
  `--count-matches`, `-m`/`--max-count`.
- **One code path for both engines.** The scanner and the index's verifier already run the
  same `grep-searcher` setup (`mtseek-walk`'s `search_bytes`). Context lines, each line's byte
  offset and each match's position come from that same `Sink`. A hit gains:
  - the byte offset of each line
  - the start and end of each match within its line
  - context lines, marked as such
- **Globs and types follow ripgrep's rules, which aren't only narrowings.** Measured with
  ripgrep 15 (and pinned by `mtseek-walk`'s unit tests):
  - A glob is checked before ignore files and hidden names. A file that matches a glob
    without `!` is searched even if it is ignored or hidden, in every folder the walk enters
    (`-g '*.log'` finds gitignored logs). With any such glob, other files are left out. A
    folder that matches a glob without `!` is entered even if ignored or hidden; one that
    matches a `!` glob is not entered. Later globs win; `--iglob`s come after all `-g`s.
  - A `-t` type is checked after ignore files but before hidden names, so it lists hidden
    files of that type. `-T` only leaves files out. Later `-t`/`-T` win.
  - Globs are relative to the working directory, not the search root: a path inside it is
    matched without it, any other path whole. The daemon gets the client's working
    directory and uses it.
- **On the index path they are a path check** on each candidate, applied before
  verification with the walker's own matchers (`mtseek-walk`'s `Rules`). The index answers
  only when it holds every file the rules can list (`mtseek_query::unanswerable`):
  - a glob without `!` needs an index built with `-uu` (it holds ignored and hidden files),
    and no glob may name a folder the search wouldn't enter;
  - a `-t` needs an index that holds hidden files;
  - `!` globs and `-T` narrow, so any index answers them.

  Otherwise the root is scanned. So does a search with a `!` glob where the index recorded an
  unreadable folder, since working out which error lines remain isn't worth the code.
  An index built with default filters therefore scans most glob and type searches. Recording
  the names of ignored and hidden files in the folders an index enters would let it answer
  them; that changes the index format and its catch-up, so it gets its own decision.
- **Out of scope:** `-U` (multiline), `-r` (replace), `-o` (only matching), `--sort`. The
  planner can't yet plan a pattern that spans lines; `-U` searches keep failing with a clear
  error rather than being slow or wrong.

### 3. `--json`: ripgrep's JSON Lines, message for message (M4a)

- **The same messages as `rg --json`:** `begin`, `match`, `context`, `end` per file, then one
  `summary`, with the same fields. Text that isn't valid UTF-8 is `{"bytes": "<base64>"}`,
  as in ripgrep, for lines and paths alike.
- **`stats` has ripgrep's keys; the values are mtseek's own.** `matched_lines`, `matches` and
  `searches_with_match` must equal ripgrep's. `elapsed`, `bytes_searched`, `bytes_printed`
  and `searches` describe what mtseek did. An index search reads few files, so these differ
  by design.
- **Combinations follow ripgrep:** where `rg` lets `-l` or `-c` override `--json`, so does
  mtseek. The parity test checks each combination against `rg`.
- **The parity suite compares JSON message by message**, ignoring only the values listed
  above. In direct mode and through the daemon, as for every other output.

### 4. Pipe protocol version 2

- **The new options and hit fields go into `search`.**
- **They need a version bump, despite 0013's rule.** 0013 lets optional fields be added
  without a new version, because both sides ignore fields they don't know. A daemon that
  ignored `glob` would return files the user excluded. Version 2 makes the new options
  binding:
  - The CLI offers versions 1–2.
  - A search that uses an option from §2 needs version 2. Against a version 1 daemon it runs
    in direct mode, as for any other decline.
  - From now on, an option that changes what matches always bumps the version.

### 5. The MCP server (M4b)

- **Its own crate and binary**, `mtseek-mcp`, as ARCHITECTURE has planned since M0. It is
  registered in an agent's configuration, for example
  `claude mcp add mtseek -- mtseek-mcp`, and runs as the user, for the agent's session.
- **Transport: stdio only.** Newline-delimited JSON-RPC on stdin and stdout, the same framing
  as the pipe. Logs go to stderr. No HTTP transport: a local search tool has no reason to
  listen on a port (0013 rejected TCP for the same reason).
- **It speaks both protocol versions:**
  - 2025-11-25: `initialize`, `notifications/initialized` and `ping`.
  - 2026-07-28: `server/discover`, the version in each request's `_meta`, `resultType` on
    every result, and `ttlMs` and `cacheScope` on `tools/list`.

  It answers in the version the client uses, and rejects any other version with the spec's
  error naming those it supports. Exact field shapes come from each version's published
  schema, and the protocol tests check them.
- **Written on `serde_json`, without an SDK.** `rmcp` brings an async runtime that nothing
  else in mtseek uses, and its 2026-07-28 support is in beta. The server needs three tools,
  one transport and two handshakes, on the framing `mtseek-ipc` already has.
- **Cancellation:** `notifications/cancelled` stops a running search through the pipe's
  `$/cancel`, or the in-process engine's stop.

### 6. Tools

All three are read-only (`readOnlyHint: true`). Paths are relative to the server's working
directory: the folder the agent starts it in, normally the project. Roots, the MCP feature
for telling a server its folders, are deprecated in 2026-07-28, so they aren't used. Each result is ripgrep-style text in a `text`
content block, which models read well and cheaply. The same data goes in `structuredContent`,
described by an `outputSchema`.

- **`search`**: content search, the CLI's semantics.
  - **Parameters:** `pattern`; `paths`; `fixed_strings`; `case` (`sensitive`, the default
    as in ripgrep, `insensitive` or `smart`); `glob` and `type` (lists); `output` (`lines`,
    `files` or `count`); `before`, `after` and `context`; `unrestricted` (0–3) or `hidden`
    and `no_ignore`; `max_results`.
  - **Bounded:** at most `max_results` lines or files come back (200 by default). Lines are
    cut at 500 bytes around the first match, and the cut is marked. The result says how
    many matched in all, and whether it was cut.
  - **Ordered by path**, then line, so the same search returns the same results. A cut keeps
    the first entries in that order. That needs the whole search to finish, which is cheap
    from an index. On the scanner path the server stops collecting at 10,000 lines and says
    the total is "at least" that.
- **`find_files`**: name search, the `--names` engine (the MFT as administrator, a walk
  otherwise). Space-separated terms, an optional glob on the path, `paths`, `max_results`.
- **`index_status`**: which folders have a content index, how current each one is, and
  whether a daemon or the service answers. It tells an agent whether a search will be fast.

There is no tool that builds an index or writes anything. Indexing a folder costs seconds to
minutes and leaves files in the cache, so a person decides it (`mtseek --index`).

### 7. Where searches run

- **The shared client side moves into a new crate, `mtseek-client`:** "the daemon over the
  pipe when one answers, else the engine in this process", with the declines of 0014 and
  the protocol choice of §4. It moves out of `mtseek-cli/src/daemon.rs`, so the CLI and the
  MCP server can't drift apart.
- **The MCP server prefers a daemon** (the user's, then the service), like the CLI.
- **Without one, it keeps a resident engine for the session.** It holds indexes open and
  catches up from the journal between calls, so repeated searches cost what they would
  through a daemon. It saves indexes the way the CLI's direct mode does, so the two never
  conflict.

### 8. Security

- **The server is a client, running as the user.** It sees what the user's own `rg` would,
  through the same checks as every other host (0014): its own token in-process, its token
  over the pipe.
- **No network transport, no writing tools,** as above.
- **File contents are untrusted data for the agent.** `docs/mcp.md` says so; the server adds
  no instructions of its own to results.

### 9. Tests and measurement

- **M4a:**
  - The parity suite gains the flags of §2 and `--json`, in direct and daemon mode, compared
    with `rg`.
  - The oracle test (index = scanner) gains globs, types and context.
- **M4b:**
  - Protocol tests drive `mtseek-mcp` over stdio as a 2025-11-25 client and as a 2026-07-28
    client: discovery, `tools/list`, every tool, errors, cancellation, and an unsupported
    version.
  - Each `search` result is compared with `rg --json --sort path` for the same parameters,
    with the same cut applied.
  - The request parser gets a property test on damaged input, like the pipe's.
- **End to end with Claude Code:** a script registers the server, runs `claude -p` on
  questions that need a search, and checks in the `stream-json` output that
  `mcp__mtseek__search` was called and its answer used. It needs Claude credentials, so it
  runs locally before M4b merges, not in CI. Its result goes in the PR.
- **Benchmark:** `tools/call search` round trips for the benchmark's queries, against `rg`
  and against the CLI through the daemon.

## As built (M4a)

- **Flag combinations, measured against ripgrep 15:**
  - `-l`, `-c`, `--count-matches` and `--json` override each other; the last one given wins.
  - `-A` and `-B` win over `-C` whatever the order. A flag given twice takes its last value.
  - `-m` is the searcher's limit, as in ripgrep: the context after the last allowed match is
    still printed, a match inside it is printed and counted as a match (`-c -m1 -A2` can
    count 2), and the searcher gets context and `-m` in every output mode.
  - With context, `--` separates groups of lines and files.
- **Match positions** (`--json`, `--count-matches`) are found as ripgrep's printers find them:
  in the line without its `\n`, an empty match at the very end of a line without a `\n` left
  out.
- **`--json` is byte for byte ripgrep's**: per-file messages in ripgrep's field order, the
  summary's keys sorted. The parity test compares `bytes_printed` too, which is equal only
  if every message is.
- **One scanner for both paths.** Context lines, offsets, positions and per-file counts all
  come from `FileSearcher`'s `Sink`, so the index's verifier reports exactly what the scanner
  reports. The oracle test covers every output mode, context, `-m` and random globs and
  types.
- **Protocol version 2** carries everything above: `SearchParams::version` says which version
  a search needs, and the CLI runs a search a version 1 daemon can't carry in this process,
  with the reason in `--stats`.
- **Finer ignore flags** (after M4a): `--no-ignore-vcs`, `--no-ignore-dot`,
  `--no-ignore-parent`, `--no-ignore-global`, `--no-ignore-exclude`, `--ignore-file` and
  `--no-ignore-files`, mapped onto the walker as ripgrep maps them. `--ignore-file` applies
  even with `-u`, and one that can't be read is reported without making the exit code 2.
  No index records these walks (it records all ignore files or none), so they scan, and
  the CLI runs them itself: the protocol doesn't carry them until an index can answer them.
  Since [0016](0016-left-out-names.md), `--no-ignore-vcs` is a walk the index records
  (protocol 3), and globs and types are answered by indexes built with default filters.

## Alternatives considered

- **Only the CLI, no MCP server.** Agents can already shell out to `mtseek`. But each call
  starts a process, output has no limit, and the agent parses text. An MCP tool returns
  bounded, ordered, structured results, and agents use registered tools without being told
  how. The CLI work (M4a) serves both.
- **`rmcp`, the official SDK.** Less code to write, but an async runtime for a process that
  handles one request at a time, and beta support for the current spec. Revisit if the tool
  surface grows, or if a third protocol version arrives.
- **Only 2026-07-28.** Simpler, but today's clients would fail to connect.
- **`mtseek --mcp` instead of a new binary.** One file fewer to ship, but the CLI's flags are
  ripgrep's contract, and a mode that turns stdin and stdout into a protocol doesn't belong
  among them. `mtseek mcp` would clash with searching for the word "mcp".
- **Streamable HTTP.** It serves remote and multi-client setups. A local tool that runs as
  the user needs neither, and a port is reachable by every local user.
- **An `index` tool.** It would let an agent make its own searches fast, but writing gigabytes
  of cache isn't something an agent should decide alone. It can come later behind an
  explicit server setting.
- **Only `structuredContent`.** Some clients show models only the content blocks. Text
  works everywhere.

## Consequences

- **New crates:** `mtseek-client` (shared client side) and `mtseek-mcp` (the server). No new
  external dependencies.
- **Dependency rule:** the direct-mode exception moves from `mtseek-cli` to
  `mtseek-client`, which may run `mtseek-engine` in-process. The CLI and the MCP server
  depend on `mtseek-client` and `mtseek-ipc`, never on the engine's internals.
- **Pipe protocol version 2**, and the rule that options which change what matches always
  bump the version.
- **`docs/mcp.md`:** setup for Claude Code and other clients, the tools and their limits, and
  a note on untrusted file contents.
- **CI time grows**: more parity cases, run twice (direct and daemon).
- **Multiline search stays unsupported.** Agents fall back to their own tools for it.
- **Globs and types are fast only on wide indexes, for now.** With an index built with
  default filters, `-g '*.rs'` and `-t rust` scan (§2). That is the common agent search, so
  the follow-up decision on recording ignored and hidden names comes before M4b's benchmark.
