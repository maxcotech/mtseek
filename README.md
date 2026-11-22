# mtseek

**Instant full-disk search for Windows.** Type anything and see every match on your machine,
in file names *and* file contents, in under 100 ms. Results are always up to date, and nothing
rescans in the background.

> **Status: pre-alpha (milestone M3, daemon in console and service mode).** The `mtseek` CLI
> gives ripgrep-identical results. File-name search (`--names`) is indexed and kept current.
> Content search answers from a content index once you create one with `--index`: 5–16× faster
> than ripgrep on the benchmark, and kept current from the NTFS change journal. `mtseekd`
> keeps the indexes open between searches, for one user (`--console`) or as a service for
> every user of the machine, each seeing only what their account may read. See the
> [roadmap](#roadmap).

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
mtseek -C2 -g "*.rs" "fn main" C:\Dev      # context lines, only Rust files
mtseek --json -t py "import os" C:\Dev     # rg's JSON Lines, only Python files
mtseek --names "paystack .cs" C:\Dev       # file names: every term must match
mtseek --index -l -F Paystack C:\Dev       # build a content index for C:\Dev; later searches use it
```

`--names` keeps its index in `%LOCALAPPDATA%\mtseek\cache` and updates it from the NTFS change
journal on each run, so only the first run on a folder takes seconds. Run it once as administrator
to index the whole drive from the master file table; later runs don't need admin.
Deleting the cache is always safe.

`--index` builds a content index of the folders and filters you search, in the same cache:
about 1.5 s and 38 MB for a 15,000-file source tree. From then on every content search of
those folders, or of any folder inside them, uses it, as long as its filters list no more
files (`-uu` covers `-u`, `--hidden` and the defaults). Each run first applies what the change journal recorded since the
last one (about 1 ms if nothing changed there, about 40 ms after an edit), then reads the
index instead of your files. `--no-index` scans instead, `--reindex` rebuilds. Changes to
`.gitignore`, `.ignore`, `.git/info/exclude` and ignore files in folders above are picked up
too; git's global excludes file only takes effect after `--reindex`.

`mtseekd --console` (install it with `cargo install --path crates/mtseek-daemon`) keeps the
indexes open and current while it runs, as your user, with no admin rights. `mtseek` answers
through it when it is running and prints exactly the same results; most indexed searches
of the 15,000-file benchmark tree then take 44–57 ms including process start. `mtseekd --status`
shows what it holds, `mtseekd --stop` writes what is in memory and stops it, and
`mtseek --no-daemon` searches without it.

`mtseekd --install`, from an elevated terminal, registers the `mtseek` service instead: one
daemon for the whole machine, running as LocalSystem, with its indexes in
`%ProgramData%\mtseek` (readable by SYSTEM and Administrators only). Every user's `mtseek`
answers through it and sees exactly what their own `rg` would: lines from files their account
may read, "Access is denied" for the rest. The service answers only what an index answers
exactly for the caller; anything else (a folder it doesn't index, `-L`, a user with a git
global excludes file) is searched in the user's own process. `mtseekd --uninstall` removes it.
Secret-looking files (`.env*`, `id_rsa*`, `*.pem`, `*.key`, `*.pfx`, ...) are never stored in
any index; searches read them from disk.

The flags are ripgrep's (`-F -i -S -s -l -c --count-matches -n -N -H -I -A -B -C -m -g --iglob -t -T
-u -uu -uuu --hidden --no-ignore -L -j --json`), and so is the output: `path:line:text`, or
`rg --json`'s JSON Lines message for message. Editors, scripts and AI agents that already speak
`rg` work unchanged. That's a tested contract
([ADR 0005](docs/adr/0005-ripgrep-compatible-contract.md), [ADR 0015](docs/adr/0015-agents-json-and-mcp.md)).
As in ripgrep, a file that matches `-g` is searched even if it is ignored or hidden, so the
index answers a glob or `-t` search only if it was built with `-uu`; otherwise it scans.

## Roadmap

| | Milestone | |
|---|---|---|
| M0 | Foundation: workspace, CI, benchmark baseline, ripgrep parity tests | ✅ |
| M1 | Name engine: MFT + USN journal, instant file-name search | ✅ |
| M2 | Content engine: trigram index + content store, kept current, under 100 ms | index ✅, live ✅; 12 of 16 benchmark queries under 100 ms |
| M3 | Background service, live updates, per-user access control | coverage roots ✅, console daemon ✅, service mode ✅ |
| M4 | MCP server for AI agents, `--json` | agent flags and `--json` ✅ |
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
