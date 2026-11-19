# mtseek

**Instant full-disk search for Windows.** Type anything and see every match on your machine,
in file names *and* file contents, in under 100 ms. Results are always up to date, and nothing
rescans in the background.

> **Status: pre-alpha (milestone M0, foundation).** The `mtseek` CLI works today and gives
> ripgrep-identical results, but it still *scans*. That makes it as fast as ripgrep, not faster
> yet. The index arrives in M2. See the [roadmap](#roadmap).

## Why

On a typical dev laptop, ripgrep, the fastest scanner there is, needs **~0.6 s** to search
a 579 MB source tree and **~3 minutes** to search everything under `C:\Dev` (1M files, 43.6 GB).
The time barely depends on the query, because the cost is opening files: NTFS plus Windows
Defender charge 38–175 µs per open. Measurements: [`bench/TARGETS.md`](bench/TARGETS.md).

mtseek doesn't open your files at query time:
- It reads the NTFS Master File Table directly, so listing every file takes seconds.
- It keeps up with every change through the USN journal, so it never rescans.
- It answers content queries from a trigram index with a compressed content store.

[How it works →](docs/ARCHITECTURE.md)

## Use it

```powershell
cargo install --path crates/mtseek-cli   # until there's an installer
mtseek -F "ConnectionString" C:\Dev        # same flags and output as ripgrep
mtseek -l -i paystack C:\Dev               # files only, case-insensitive
mtseek -uu "TODO\(\w+\)" C:\Dev            # include ignored + hidden files
```

The flags are ripgrep's (`-F -i -S -s -l -n -N -H -I -u -uu -uuu --hidden --no-ignore -L -j`), and the
output is ripgrep's `path:line:text`. Editors, scripts and AI agents that already speak `rg`
work unchanged. That's a tested contract ([ADR 0005](docs/adr/0005-ripgrep-compatible-contract.md)).

## Roadmap

| | Milestone | |
|---|---|---|
| M0 | Foundation: workspace, CI, benchmark baseline, ripgrep parity tests | ✅ |
| M1 | Name engine: MFT + USN journal, instant file-name search | next |
| M2 | Content engine: trigram index + content store, under 100 ms | |
| M3 | Background service, live updates, per-user access control | |
| M4 | MCP server for AI agents, `--json` | |
| M5 | Desktop app (GPUI): global-hotkey palette with preview | |
| M6 | Signed installer, winget, auto-update | |

## Develop

```powershell
cargo xtask ci        # fmt, clippy (warnings = errors), tests incl. ripgrep parity, docs
cargo xtask bench     # release build + benchmark against ripgrep on C:\Dev
```

Start with [`AGENTS.md`](AGENTS.md). It's written for AI coding agents and works just as well
for humans. Then read [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md).

## License

[MIT](LICENSE)
