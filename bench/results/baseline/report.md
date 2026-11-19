# seek benchmark report

Run 2026-11-19T21:41:07 · 11th Gen Intel(R) Core(TM) i7-1185G7 @ 3.00GHz · 8 logical CPUs · 31.7 GB RAM · Windows-11-10.0.26200-SP0  
on AC power · 5 timed runs per cell after 1 warmup · process spawn floor 13 ms · target 100 ms

Times are wall-clock medians, warm OS file cache (the best case for a scanning tool). `first` is the first run of the cell, which is not guaranteed to be a cold cache.

## Contenders

| name | version | note |
|---|---|---|
| rg (reference) | ripgrep 15.2.0 (rev e89fff89ac) | Baseline. Scans files at query time. |

## Corpus

| scope | root | rg flags | files | bytes in scope | `rg --files` alone |
|---|---|---|---:|---:|---:|
| dev | `C:\Dev` | `(defaults)` | 15,086 | 579.4 MB | 123 ms |
| all | `C:\Dev` | `-uu` | 1,052,796 | 43.6 GB | 5.42 s |

## Queries

| id | pattern | kind | what it tests |
|---|---|---|---|
| absent | `zq7xkvq9plmw` | literal | No matches. Worst case for a scanner (reads everything), best case for an index. |
| rare | `Paystack` | literal | ~17 files. The typical 'where is that thing' search. |
| uncommon | `HttpClient` | literal | ~80 files. |
| medium | `useEffect` | literal | ~800 files. |
| common | `import` | literal | Most files match. Index gives little; output volume dominates. |
| icase | `paystack` | literal, ignore case | Case-insensitive literal. |
| regex-lit | `Connection\w*String` | regex | Regex with literal anchors; an index can use 'Connection' and 'String'. |
| regex-nolit | `\b\d{3}-\d{3}-\d{4}\b` | regex | No useful literal. Index-hostile: honest case where an index may not help. |

## Results: `dev`

| query | mode | matches | rg median | rg min–max | rg throughput | first |
|---|---|---:|---:|---:|---:|---:|
| absent | files | 0 files | 647 ms | 621 ms–683 ms | 0.88 GB/s | 2.03 s |
| absent | lines | 0 lines | 645 ms | 611 ms–792 ms | 0.88 GB/s | 611 ms |
| rare | files | 17 files | 608 ms | 550 ms–705 ms | 0.93 GB/s | 636 ms |
| rare | lines | 30 lines | 585 ms | 560 ms–656 ms | 0.97 GB/s | 676 ms |
| uncommon | files | 82 files | 535 ms | 522 ms–605 ms | 1.06 GB/s | 524 ms |
| uncommon | lines | 194 lines | 554 ms | 523 ms–598 ms | 1.02 GB/s | 592 ms |
| medium | files | 779 files | 568 ms | 528 ms–607 ms | 1.00 GB/s | 537 ms |
| medium | lines | 1,740 lines | 606 ms | 566 ms–635 ms | 0.93 GB/s | 609 ms |
| common | files | 9,401 files | 599 ms | 592 ms–646 ms | 0.94 GB/s | 604 ms |
| common | lines | 52,995 lines | 594 ms | 571 ms–621 ms | 0.95 GB/s | 690 ms |
| icase | files | 17 files | 550 ms | 505 ms–577 ms | 1.03 GB/s | 562 ms |
| icase | lines | 33 lines | 515 ms | 511 ms–567 ms | 1.10 GB/s | 520 ms |
| regex-lit | files | 37 files | 533 ms | 506 ms–562 ms | 1.06 GB/s | 544 ms |
| regex-lit | lines | 79 lines | 543 ms | 516 ms–571 ms | 1.04 GB/s | 558 ms |
| regex-nolit | files | 3 files | 544 ms | 520 ms–572 ms | 1.04 GB/s | 546 ms |
| regex-nolit | lines | 7 lines | 555 ms | 525 ms–595 ms | 1.02 GB/s | 521 ms |

**rg geomean: 573 ms** · 0/16 cells under the 100 ms target.

## Results: `all`

| query | mode | matches | rg median | rg min–max | rg throughput | first |
|---|---|---:|---:|---:|---:|---:|
| absent | files | 0 files | 190.41 s | 187.09 s–193.74 s | 0.23 GB/s | 192.76 s |
| rare | files | 30 files | 181.51 s | 178.25 s–184.76 s | 0.24 GB/s | 192.87 s |
| common | files | 400,769 files | 179.53 s | 170.32 s–188.73 s | 0.24 GB/s | 188.14 s |

**rg geomean: 183.75 s** · 0/3 cells under the 100 ms target.
