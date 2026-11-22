# Architecture decision records

One file per decision that is expensive to reverse. Format: context → decision → consequences.
Never edit an accepted ADR's decision. Write a new ADR that supersedes it and link both ways.

| # | Decision | Status |
|---|---|---|
| [0001](0001-trigram-index.md) | Trigram index for content search | Accepted |
| [0002](0002-content-store.md) | Store compressed file text in the index | Accepted |
| [0003](0003-gpui-desktop-app.md) | GPUI for the desktop app | Accepted, pending spike |
| [0004](0004-service-security.md) | SYSTEM service that filters results by the caller's ACLs | Accepted, made concrete by 0014 |
| [0005](0005-ripgrep-compatible-contract.md) | ripgrep is the behavioural contract | Accepted |
| [0006](0006-windows-first.md) | Windows (NTFS) first | Accepted |
| [0007](0007-name-engine.md) | Name engine: flat arenas, parent links, two sources | Accepted, partly superseded by 0008 |
| [0008](0008-persisted-name-index.md) | Persisted name index, caught up from the journal without admin | Accepted |
| [0009](0009-raw-mft-records.md) | Read raw MFT records for whole-drive indexes | Accepted |
| [0010](0010-content-index-format.md) | Content index: stored bytes, query planning, segment file | Accepted |
| [0011](0011-live-content-index.md) | Keep the content index current from the USN journal | Accepted |
| [0012](0012-index-coverage.md) | What the daemon indexes: coverage roots, narrowed at query time | Accepted, partly superseded by 0014 |
| [0013](0013-daemon-and-ipc.md) | The daemon and its pipe protocol | Accepted |
| [0014](0014-service-mode.md) | Service mode: one machine-wide daemon that shows each caller only what it could read | Accepted |
| [0015](0015-agents-json-and-mcp.md) | Agents: `--json`, the flags agents use, and an MCP server | Accepted |
| [0016](0016-left-out-names.md) | The index records what its walk leaves out, and the `--no-ignore-vcs` walks | Accepted |
