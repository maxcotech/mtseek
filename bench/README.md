# mtseek benchmark harness

Proves (or disproves) that `mtseek` beats a scanning search tool, using numbers
instead of claims. It drives every contender only through its command line,
over the same corpus and queries, and checks that they **return the same
files** before comparing speed. A fast wrong answer counts as a failure.

## Run

```powershell
python bench.py                      # default scope (dev), all queries, all installed contenders
python bench.py --scopes dev,all     # add the whole-tree scope (minutes per query)
python bench.py --queries rare,absent --modes files --runs 3
python bench.py --modes names        # file-name search: rg --files vs mtseek --names
python bench.py --list               # show scopes, queries, contenders
```

Each run writes `results/<timestamp>/report.md` (readable tables) and
`results.json` (every raw sample), both refreshed after each cell. So an
interrupted run still leaves data. Corpus sizes are cached in `results/cache/`.
Use `--refresh-corpus` after the tree changes a lot.

Python 3.10+ standard library only. ripgrep must be on `PATH`.

## What it measures

- **Wall-clock time of one CLI invocation**, as the user experiences it:
  1 warmup run, then N timed runs. Contenders run round-robin within a cell,
  so background noise is shared rather than landing on one tool.
- **Warm OS file cache.** That is ripgrep's best case. Making the cache cold on
  Windows needs admin tools (RAMMap / EmptyStandbyList), so it is not automated.
  The `first` column is the first run of each cell, which is often but not
  reliably cold.
- **Spawn floor**: the cost of starting a trivial process (`rg --version`). No
  CLI contender can go below it.
- **Throughput**: bytes in scope ÷ median time.
- **Parity**: in `files` mode, the exact set of matching paths is compared
  against the reference (`rg`). Missing or extra files fail the cell. In
  `lines` mode, matching-line counts are compared.

## Scopes

| scope | meaning |
|---|---|
| `dev` | `C:\Dev` with ripgrep defaults (respects `.gitignore`, skips hidden and binary files). ripgrep's best case. |
| `all` | `C:\Dev` with `-uu`: everything on disk, including `node_modules`, `bin/obj` and hidden files. What "search my whole machine" means. |

The corpus is defined as what `rg --files` lists with the scope's flags. To
pass parity, mtseek has to index the same file set for a scope.

## Modes

| mode | what is timed | parity |
|---|---|---|
| `files` | content search, list matching files (`rg -l`) | exact set of paths |
| `lines` | content search, print matching lines | matching-line count |
| `names` | file-name search over everything under the root | every file rg lists must appear; extras must be directories |

`names` runs only the `kind: "name"` queries, and the content modes run only the others:
`python bench.py --modes names`. ripgrep has no name search, so its side is
`rg --files -uu --iglob *pattern*`: list everything, keep the matching names. A contender
takes part in `names` only if it has a `names_argv`. Its `names_reset` directories are
emptied before each cell, so the cell's `first` run includes building any index, and the
timed runs show what a user sees from then on. mtseek's index cache lives in
`%TEMP%\mtseek-bench\mtseek-names`: not in the user's own cache, and not under any scope's
root, where it would become part of the corpus being searched.

## Queries

Chosen to span selectivity, from no matches to almost every file matching. They
include one regex with no usable literal (`regex-nolit`), where a trigram index
should *not* be expected to help. It is there to keep the comparison honest.

## Adding a contender

Add an entry under `contenders` in `bench.json`. `argv` is a template:
`{flags}` expands to the flags for the cell (`files`/`lines`, `literal`,
`ignore_case`, `scope:<name>`), and `{pattern}` / `{root}` are substituted.
Tools without a fixed-string mode can set `"literal": "escape"`. An index-backed
contender can set `env` (variables whose values are folders under
`%TEMP%\mtseek-bench`) and `reset` (folders there emptied before each cell, so
`first` includes building the index), as `mtseek-index` does. `mtseek`,
`mtseek-index` and `csearch` (Russ Cox codesearch) entries are already there. Zoekt
can be added the same way. Contenders that aren't installed are skipped automatically.

## Caveats on this machine

- Windows Defender on-access scanning is enabled. Every file a scanner opens
  goes through it, which is part of why the `all` scope is slow. That is the
  real-world condition for most Windows developers, so it is left as is.
- The CPU is a laptop part. Results taken on battery are flagged in the report
  and are not comparable to results taken on AC power.
