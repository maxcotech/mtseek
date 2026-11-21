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

## Progress

### 2026-11-20, after M1b (`cargo xtask bench --modes names` and `--modes files,lines`, 5 timed runs)

**File names** (`mtseek --names` vs `rg --files -uu --iglob *pattern*`, everything under
`C:\Dev`, 1.06M files). Parity ✓ in every cell (mtseek also lists matching directories).

| query | matching files | rg median | mtseek median | speedup | first run (mtseek builds its index) |
|---|---:|---:|---:|---:|---|
| name-absent `zq7xkvq9plmw` | 0 | 4.31 s | 126 ms | 34× | rg 4.14 s · mtseek 4.29 s |
| name-rare `paystack` | 7 | 4.77 s | 170 ms | 28× | rg 4.21 s · mtseek 4.12 s |
| name-medium `controller` | 1,825 | 4.95 s | 194 ms | 26× | rg 4.82 s · mtseek 4.69 s |
| name-common `.json` | 38,309 | 5.34 s | 381 ms | 14× | rg 5.05 s · mtseek 5.16 s |

The first run costs the same as rg (both walk the tree). After that, mtseek loads its saved
index (~100 ms) and catches up from the change journal. Still above the 100 ms target:
loading the index from disk on every run is the floor until the daemon (M3) keeps it in memory.

**Content** (`dev` scope). Parity ✓ in all 16 cells. rg geomean 750 ms this run (573 ms
in the baseline above; the tree grew to 15,134 files, and the machine was less idle).

| mode | rg medians | mtseek medians | speedup |
|---|---|---|---|
| files | 562–807 ms | 319–446 ms | 1.6–1.8× |
| lines | 739–908 ms | 425–570 ms | 1.5–1.9× |

Don't count this as progress toward the target yet. mtseek's content search is still the
reference scanner, built on ripgrep's own crates and opening every file like rg does. The
gap is real and repeatable (M0 measured it too), but nobody has explained it yet. Candidates:
thread count, rg's per-file work that mtseek skips, and build flags (mtseek is built with
thin LTO and one codegen unit). The content target needs the trigram index (M2).

### 2026-11-20, listing everything (`--names ""`)

Listing every name used to build each full path and then sort them with `Path`'s
component-by-component comparison. That took 9.1 s of search time for the 1.2M entries
under `C:\Dev` (from `--stats`), slower than `rg --files`. Now entries are grouped by
folder, each folder's names are sorted when the walk enters it, and a depth-first walk
emits paths already in order:

| `mtseek --names "" C:\Dev` | search time (`--stats`) |
|---|---:|
| before | 9,118 ms |
| after | 696–905 ms |

`cargo xtask bench --modes names` after the change, with a busier machine than the run
above (rg medians 7.0–7.6 s instead of 4.3–5.3 s). Parity ✓ in every cell.

| query | matching files | rg median | mtseek median | speedup |
|---|---:|---:|---:|---:|
| name-absent | 0 | 7.63 s | 227 ms | 34× |
| name-rare | 7 | 7.39 s | 294 ms | 25× |
| name-medium | 1,825 | 7.45 s | 316 ms | 24× |
| name-common | 38,317 | 7.01 s | 329 ms | 21× |
| name-all (new) | 1,060,855 | 7.49 s | 1.45 s | 5.2× |

### 2026-11-20, M1c: whole-drive index from raw MFT records (admin)

Measured with `mtseek --names --stats` in an elevated terminal, since the benchmark harness
doesn't run elevated.

| whole-drive build of `C:` | entries | build time | `C:\Dev` files missing vs `rg --files -uu` |
|---|---:|---:|---:|
| `FSCTL_ENUM_USN_DATA` (M1) | 2,433,531 | 5.6 s | 3,877 (extra hard links) |
| raw MFT records (M1c) | 2,509,479 | **2.56 s** | **0** of 1,060,747 |

### 2026-11-21, M2a: content index (`cargo xtask bench --modes files,lines`, 5 timed runs)

New contender `mtseek-index`, which is `mtseek --index` (ADR 0010). Its index is emptied
before each cell, so `first` includes building it. Parity ✓ in all 16 cells. The machine was
busy: rg's geomean was ~0.95 s, against 573 ms in the baseline.

| query | mode | matches | rg | mtseek (scan) | mtseek-index | speedup vs rg | first (index build + search) |
|---|---|---:|---:|---:|---:|---:|---:|
| absent | files | 6 | 625 ms | 401 ms | **35 ms** | 17.7× | 1.89 s |
| rare | files | 27 | 911 ms | 626 ms | **72 ms** | 12.6× | 1.95 s |
| uncommon | files | 87 | 1.08 s | 617 ms | **59 ms** | 18.2× | 2.91 s |
| medium | files | 788 | 1.18 s | 707 ms | 124 ms | 9.5× | 2.55 s |
| common | files | 9,474 | 1.01 s | 521 ms | 101 ms | 10.0× | 3.53 s |
| icase | files | 30 | 982 ms | 629 ms | **71 ms** | 13.8× | 3.07 s |
| regex-lit | files | 42 | 928 ms | 575 ms | **73 ms** | 12.8× | 2.17 s |
| regex-nolit | files | 4 | 1.01 s | 616 ms | 166 ms | 6.0× | 2.00 s |
| absent | lines | 11 | 892 ms | 536 ms | **52 ms** | 17.1× | 2.15 s |
| rare | lines | 47 | 862 ms | 627 ms | **79 ms** | 11.0× | 2.15 s |
| uncommon | lines | 201 | 915 ms | 748 ms | **66 ms** | 13.8× | 2.48 s |
| medium | lines | 1,754 | 958 ms | 617 ms | **78 ms** | 12.2× | 2.37 s |
| common | lines | 53,342 | 998 ms | 581 ms | 178 ms | 5.6× | 2.42 s |
| icase | lines | 67 | 819 ms | 608 ms | **83 ms** | 9.9× | 2.40 s |
| regex-lit | lines | 89 | 1.08 s | 709 ms | 100 ms | 10.7× | 2.64 s |
| regex-nolit | lines | 8 | 949 ms | 643 ms | 194 ms | 4.9× | 2.83 s |

Bold cells are under the 100 ms target: 10 of 16. (`absent` isn't absent any more: the token
now appears in this repo's own docs and tests.)

The index, measured with `--stats`:

- **Build:** 1.75 s for 15,399 files. 102.7 MB of stored text compresses to 20.1 MB. There are
  121,611 trigrams and 14.3 M postings, and the file is **37.3 MiB** on disk. Binary files
  are cut after their first NUL, which is what keeps 693 MB of files down to 103 MB of text.
- **Opening the index:** about 3 ms.
- **Engine time per query** (plan, postings, decompress and verify):
  - `Paystack`: 16 ms (168 candidates)
  - `useEffect`: 21 ms (840 candidates)
  - `import`: 36–50 ms (9,645 candidates)
  - `regex-nolit`: 70 ms. It has no trigram to narrow by, so it decompresses and searches
    all 1,119 blocks.

  The rest of the wall time is process start (11 ms floor), thread start-up and writing the
  output. So the over-100 ms cells are output volume (`common`), the honest worst case
  (`regex-nolit`), and noise (`medium` files varied from 90 to 154 ms on this run).
- **Not yet covered:** the index is built once and not updated (M2b). Update latency, pass
  criterion 3, has no number until then.

### 2026-11-21, M2b: the index stays current (`cargo xtask bench --modes files,lines`, 5 timed runs)

From M2b on, every timed `mtseek-index` run first catches the index up from the change journal
(ADR 0011). Parity ✓ in all 16 cells. The machine was still busy (rg 0.78–1.0 s).

| query | mode | matches | rg | mtseek (scan) | mtseek-index | speedup vs rg |
|---|---|---:|---:|---:|---:|---:|
| absent | files | 6 | 899 ms | 633 ms | **58 ms** | 15.4× |
| rare | files | 28 | 889 ms | 508 ms | **72 ms** | 12.4× |
| uncommon | files | 87 | 908 ms | 501 ms | **59 ms** | 15.4× |
| medium | files | 788 | 845 ms | 566 ms | **83 ms** | 10.2× |
| common | files | 9,474 | 878 ms | 556 ms | 101 ms | 8.7× |
| icase | files | 30 | 808 ms | 572 ms | **69 ms** | 11.7× |
| regex-lit | files | 42 | 869 ms | 511 ms | **78 ms** | 11.2× |
| regex-nolit | files | 4 | 997 ms | 723 ms | 284 ms | 3.5× |
| absent | lines | 11 | 812 ms | 511 ms | **93 ms** | 8.8× |
| rare | lines | 49 | 955 ms | 568 ms | **64 ms** | 15.0× |
| uncommon | lines | 201 | 948 ms | 651 ms | **55 ms** | 17.3× |
| medium | lines | 1,755 | 857 ms | 507 ms | **66 ms** | 13.1× |
| common | lines | 53,343 | 849 ms | 564 ms | 132 ms | 6.4× |
| icase | lines | 69 | 781 ms | 554 ms | **64 ms** | 12.2× |
| regex-lit | lines | 89 | 790 ms | 549 ms | **73 ms** | 10.9× |
| regex-nolit | lines | 8 | 845 ms | 562 ms | 147 ms | 5.8× |

12 of 16 cells are under 100 ms. The `regex-nolit` files cell is noise: its samples ranged
from 157 to 334 ms, while the identical work in lines mode held at 146–149 ms.

**Update latency** (pass criterion 3), measured with `--stats` on `C:\Dev` (15,405 files),
catch-up time on top of the search:

| what changed since the last search | catch-up |
|---|---:|
| nothing under the scope (records about the rest of the drive only) | **1.2–1.3 ms** |
| one file created, edited or deleted | **38–40 ms** |

A new file is found by the very next search, and a deleted one is gone from it.

- **Where the 38–40 ms goes:**
  - load the editable index state: 6–8 ms
  - re-list the folder and read the file: 4–5 ms
  - write the delta, including fsync: 6 ms
  - reopen the delta: 10 ms. My guess is that Defender scans the just-written file; not
    measured.
- **What got it there:**
  - The first version took 75 ms even when nothing relevant changed, because it built its
    path map before checking any record. It now compares file reference numbers first.
  - That path map took 38 ms for 15,000 `PathBuf` keys. Keys now compare as raw bytes with
    separators lowest: the same order, compared with `memcmp`.
- **Build:** the base build lists every folder once more, for folder and control-file
  reference numbers, with no measurable cost. Warm rebuilds take 1.34–1.38 s (M2a: 1.75 s,
  measured once, colder). The first build after the cache had gone cold took 3.35 s.

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
