# 0001: Trigram index for content search

**Status:** Accepted, 2026-11-19

## Context

A scanner pays per file opened, whatever the query. The ripgrep baseline (`bench/TARGETS.md`)
costs ~570 ms on 15k files and ~3 min on 1M files, even when nothing matches. To answer in under
100 ms, a query has to touch only the files that can match.

## Decision

Index every distinct byte trigram in each file's text (case-folded for ASCII). Translate each
query into a boolean trigram query over `regex-syntax`'s HIR, following Russ Cox's
*Regular Expression Matching with a Trigram Index*. Intersect and union posting lists, then
verify each candidate with the real matcher.

Storage is LSM-style:
- immutable, memory-mapped **segments**, plus one small in-memory segment for recent changes
- background **merges** combine segments
- deletes are recorded in **tombstone bitmaps**
- a **manifest** lists the live segments and is committed by write-new + fsync + rename

## Consequences

- Rare and absent queries become near-free: `absent` resolves from the dictionary without
  touching a single document.
- Patterns with no required literal (`\b\d{3}-\d{3}-\d{4}\b`) gain nothing from the index.
  They fall back to a verifier pass over the content store (ADR 0002). That must be reported
  honestly in benchmarks, not hidden.
- Patterns shorter than 3 bytes can't use trigrams. They use the same fallback.
- The planner is subtle code. It gets property tests against the reference scanner
  (ARCHITECTURE invariant 2) and fuzzing.

## Alternatives considered

- **Suffix arrays.** Faster for some regexes, but 4–8× the corpus size and painful to update incrementally.
- **Inverted word index (Lucene-style).** Can't answer substring or regex queries, and code search needs both.
- **Faster scanning only** (SIMD, I/O tuning). Can't beat the per-file open cost the benchmark exposed.
