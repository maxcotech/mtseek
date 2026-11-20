# 0009: Read raw MFT records for whole-drive indexes

**Status:** Accepted, 2026-11-20. Closes the hard-link gap recorded in
[0008](0008-persisted-name-index.md).

## Context

Whole-drive indexes were built with `FSCTL_ENUM_USN_DATA`, which reports **one name per
file**. On `C:\Dev` that left out 3,877 files that `rg --files -uu` lists, all extra hard
links (Gradle/CMake build outputs linked into up to 20 directories). That breaks
invariant 7, name search is complete.

Every name of a file is stored in its MFT record, as one `$FILE_NAME` attribute per link.
Reading the records directly is how Everything indexes a drive. It needs the same admin
rights as the enumeration.

## Decision

- **Read the `$MFT` stream off the volume.** Find it through `FSCTL_GET_NTFS_VOLUME_DATA`
  and the run list in record 0. Read it in 4 MiB chunks, one thread reading while another
  parses. A `$MFT` fragmented beyond record 0 (an `$ATTRIBUTE_LIST` naming further records)
  is followed.
- **Parse records in safe code** (`mtseek-ntfs::mft`):
  - check and undo fixups (the update sequence array)
  - skip free records
  - emit every non-8.3 `$FILE_NAME`
  - names in extension records belong to the base record's file
  - directories are flagged by `FILE_NAME_INDEX_PRESENT`

  Records that fail their fixup check are counted and skipped. NTFS metadata (records
  below 24) is left out, since directory listings never show it.
- **Make up for lag.** NTFS writes record changes to disk lazily, so a raw read can miss
  the last seconds of changes. The build replays the USN journal from **60 seconds before
  the read** (records carry timestamps) and catches up at once. It reuses the re-listing
  catch-up of 0008. A file created during or just before the build is therefore indexed.
- **Fall back** to `FSCTL_ENUM_USN_DATA` when the raw records can't be used:
  - an unusual geometry
  - a run list with gaps
  - a non-resident attribute list
  - more than 0.1% of records failing to parse

  `--stats` names which method was used.

## Consequences

- Hard links are complete in whole-drive indexes, as they already were in walked ones.
- Correctness rests on admin-only tests: CI is elevated and `MTSEEK_REQUIRE_ADMIN=1` fails
  any skip.
  - `raw_mft_lists_every_hard_link_when_elevated` checks that the raw records name every
    link of `cmd.exe` that `fsutil hardlink list` reports.
  - `raw_mft_covers_the_enumeration_when_elevated`: every enumerated name appears in the
    raw read, with slack for churn between the two reads.
  - `mft_build_lists_every_hard_link_when_elevated` (`mtseek-names`) covers seconds-old
    links end to end, and agrees with a walk.
- Measured on the reference laptop: the raw build takes **2.56 s** for the whole of `C:`
  (enumeration: 5.6 s), with 0 unreadable records, journal replay included. It indexes
  **2,509,479 entries** (the enumeration found 2,433,531). Listing `C:\Dev` from it misses **none of the
  1,060,747 files** `rg --files -uu` lists, where the enumeration missed 3,877. Every extra
  (155,937) is a directory.
- The parser is pure byte parsing, property-tested with synthetic records (fixups, run
  lists with negative and sparse runs, attribute lists, corrupt bytes).
