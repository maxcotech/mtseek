# 0006: Windows (NTFS) first

**Status:** Accepted, 2026-11-19

## Context

The headline speed comes from NTFS-specific facilities:
- the MFT can be listed without walking directories
- the USN change journal gives a durable, ordered log of every change, so updates survive
  reboots without rescans

Comparable APIs on other platforms (fanotify on Linux, FSEvents on macOS) behave differently
enough to need their own engines.

## Decision

v1 targets Windows 10/11 on x86-64 with NTFS volumes. ReFS, FAT/exFAT and network drives use
the portable fallback (walker + `ReadDirectoryChangesW`). Platform-specific code lives behind
`cfg(windows)` in `mtseek-ntfs`; everything above it is portable Rust.

## Consequences

- CI runs on `windows-latest`. Portable crates may also be tested on Linux to keep them
  honest, but that is not a release target.
- Porting later means adding a volume-watcher crate per platform. Index, query, IPC and UI are unaffected.
