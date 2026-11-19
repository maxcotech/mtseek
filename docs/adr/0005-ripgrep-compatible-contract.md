# 0005: ripgrep is the behavioural contract

**Status:** Accepted, 2026-11-19

## Context

"Which files count, and what counts as a match" has dozens of edge cases: nested gitignores,
`require_git`, `.ignore` / `.rgignore`, hidden files, binary detection mid-file, UTF-16 BOMs,
CRLF, Unicode case folding. Users, editors and AI agents already rely on ripgrep's answers.

## Decision

For the same flags, mtseek's output equals ripgrep's. Concretely:

- The **reference scanner** (`mtseek-walk`) is built from ripgrep's own crates (`ignore`,
  `grep-searcher`, `grep-regex`) and configured the way `rg` configures them.
- The **CLI** accepts ripgrep's flag names for everything it supports (`-F -i -S -s -l -n -N -H -I
  -u -uu -uuu --hidden --no-ignore -L -j`) and prints ripgrep's `path:line:text` format.
  Unsupported flags are an error, never silently ignored.
- **Parity is tested**, not assumed. `tests/parity.rs` runs `rg` and `mtseek` on a generated
  edge-case corpus across queries × filters × output modes. CI installs `rg` and sets
  `MTSEEK_REQUIRE_RG=1`. The benchmark checks parity on the real corpus before timing anything.
- The index is a performance detail. It must never change an answer (ARCHITECTURE invariant 2).

## Known differences (deliberate)

- With no path and stdin not a terminal, `rg` searches stdin. `mtseek` searches the current directory.
- No `--heading` layout and no colour yet.
- Later: `--json` will match `rg --json`'s schema, so tools that parse it work unchanged.
