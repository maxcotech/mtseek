# 0002: Store compressed file text in the index

**Status:** Accepted, 2026-11-19

## Context

The first plan was to keep the index small by *not* storing file text, and to verify candidates
by reading the real files. The ripgrep baseline overturned that: opening a file costs
38–175 µs on this machine (NTFS + Defender's filter driver), independent of size.

| query (`dev` scope) | candidate files | opens × ~38 µs | budget |
|---|---:|---:|---:|
| `Paystack` | ~17 | < 1 ms | ✅ |
| `useEffect` | ~779 | ~30 ms | tight |
| `import` | ~9,400 | ~360 ms | ❌ 3.6× over |

On the `all` scope (~175 µs per open) even medium queries blow the budget.

## Decision

Each segment carries a **content store**: file text, transcoded to UTF-8 the way ripgrep sees it
(BOM sniffing), compressed with zstd in independently decompressible blocks. Blocks are ~64 KiB
and small files are packed together. The verifier reads candidates from the memory-mapped store,
so a query opens **zero** user files. Only dirty-set files are read from disk.

The content store is on by default and can be switched off per scope. With it off, verification
opens files, which is fine for a huge scope you rarely search.

## Consequences

- Disk cost: roughly 25–35% of indexed text for the content store, plus the trigram index.
  That's about 150–200 MB for the `dev` scope; it must be measured in M2 and shown in settings.
- The index now holds file contents, so it is as sensitive as the files themselves (ADR 0004).
- `common` on the `all` scope (400k matching files) is still bounded by decompressing and
  printing that much output. We accept that and report it.
- Open question for M2: for files-mode (`-l`) on very common literals, can positional postings
  prove a match without decompressing? Measure first.
