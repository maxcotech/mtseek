# mtseek performance targets (ripgrep baseline, 2026-11-19)

Measured on the user's machine: i7-1185G7 laptop (8 logical CPUs), 31.7 GB RAM,
Windows 11, AC power, idle. Windows Defender on-access scanning ON.
ripgrep 15.2.0, warm OS file cache (rg's best case), median of timed runs.
Process spawn floor: 13 ms.

## Corpus

Defined as what `rg --files` lists with the scope's flags. To pass parity,
mtseek must search exactly this file set per scope.

| scope | root | rg flags | files | bytes |
|---|---|---|---:|---:|
| `dev` | `C:\Dev` | (defaults: respects .gitignore, skips hidden/binary) | 15,086 | 579 MB |
| `all` | `C:\Dev` | `-uu` (incl. node_modules, bin/obj, hidden) | 1,052,796 | 43.6 GB |

## Baseline to beat

| scope | rg per search | target | speedup needed |
|---|---:|---:|---:|
| `dev` | 515–647 ms (geomean 573 ms) | < 100 ms | ~6× |
| `all` | 170–194 s | < 100 ms | ~1,800× |

`dev`, rg medians (5 runs each), files mode / lines mode:

| query id | pattern | kind | matching files | matching lines | files | lines |
|---|---|---|---:|---:|---:|---:|
| absent | `zq7xkvq9plmw` | literal | 0 | 0 | 647 ms | 645 ms |
| rare | `Paystack` | literal | 17 | 30 | 608 ms | 585 ms |
| uncommon | `HttpClient` | literal | 82 | 194 | 535 ms | 554 ms |
| medium | `useEffect` | literal | 779 | 1,740 | 568 ms | 606 ms |
| common | `import` | literal | 9,401 | 52,995 | 599 ms | 594 ms |
| icase | `paystack` | literal, -i | 17 | 33 | 550 ms | 515 ms |
| regex-lit | `Connection\w*String` | regex | 37 | 79 | 533 ms | 543 ms |
| regex-nolit | `\b\d{3}-\d{3}-\d{4}\b` | regex | 3 | 7 | 544 ms | 555 ms |

`all`, files mode only (warmup + 2 timed runs):

| query id | matching files | runs |
|---|---:|---|
| absent | 0 | 193 s, 187 s, 194 s |
| rare | 30 | 193 s, 178 s, 185 s |
| common | 400,769 | 188 s, 189 s, 170 s |

## What the numbers say

- rg's time barely depends on the query: a search for nothing costs as much
  as one matching 400k files. The cost is opening every file.
- Per-file cost is ~38 µs on `dev` but ~175 µs on `all`. The full tree is
  mostly tiny files (node_modules), and each open passes through NTFS and the
  Defender filter driver. Query-time file opens are the cost to eliminate:
  for `rare`, open ~30 candidate files, not 1M. For `absent`, open none.
- `regex-nolit` has no usable trigram. Don't expect an index to win there.
  It is in the set to keep the comparison honest.
- `common` on `all` matches 400,769 files, so output volume dominates. An
  index gains little on raw speed there.

## Pass criteria

1. **Parity**: in files mode, the exact set of matching paths equals rg's
   (paths compared case-insensitively after normalisation). In lines mode,
   the matching-line count equals rg's. The match counts above are the expected
   values. They were identical across two separate runs.
2. **Speed**: median under 100 ms per query per scope, CLI invocation included.
3. **Not yet measured, still required for a fair claim**: index build time,
   index size on disk, and update latency (file change → visible in results).
   Report these alongside query times.

## Harness

`bench.py` + `bench.json` (Python 3.10+, stdlib only), next to this file.
It already has an rg-compatible contender slot:

```json
"mtseek": { "argv": ["../target/release/mtseek.exe", "{flags}", "--", "{pattern}", "{root}"],
          "flags": { "files": ["--files-with-matches"], "lines": ["--line-number", "--no-heading", "--with-filename"],
                     "literal": ["--fixed-strings"], "ignore_case": ["--ignore-case"], "scope:all": ["-uu"] } }
```

Point `argv[0]` at the mtseek binary (relative to the bench folder, or on
PATH). Output must be one path per line in files mode, and `path:line:text` per
line in lines mode. Run with `python bench.py --scopes dev,all`. The machine
must be idle and on AC power: an earlier run during a large install was
15–30% slower.
