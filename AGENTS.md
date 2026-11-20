# AGENTS.md

Instructions for AI coding agents (and humans) working in this repo. Keep it short and true.

## What this is

mtseek: instant full-disk content search for Windows, in Rust. Read
[`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) first. It has the code map, invariants and
milestones. Decisions and their reasons are in [`docs/adr/`](docs/adr/).

## Commands

| Task | Command |
|---|---|
| Everything CI checks | `cargo xtask ci` |
| Tests only | `cargo test --workspace` |
| One crate's tests | `cargo test -p mtseek-walk` |
| ripgrep parity test | `cargo test -p mtseek-cli --test parity` (needs `rg` on PATH) |
| Format | `cargo xtask fmt` |
| Benchmark vs ripgrep | `cargo xtask bench --queries rare,absent --modes files` |
| Generate a test corpus | `cargo xtask corpus <empty-dir>` |
| Run the CLI | `cargo run -p mtseek-cli -- -F needle C:\some\dir` |

The toolchain is pinned in `rust-toolchain.toml`. Nothing needs admin rights except the MFT/USN
test in `mtseek-ntfs`, which skips when not elevated (CI is elevated and sets `MTSEEK_REQUIRE_ADMIN=1`).
To run it locally, run `cargo test -p mtseek-ntfs` from an elevated terminal.

## Rules

1. **ripgrep is the spec** (ADR 0005). If mtseek and `rg` disagree, mtseek is wrong unless an
   ADR says otherwise. Add a case to `crates/mtseek-cli/tests/parity.rs` or to the corpus in
   `mtseek-testkit` for every behaviour you touch.
2. **Numbers, not adjectives.** A performance change comes with a `cargo xtask bench` run.
   Don't write "faster" without the before/after medians.
3. **No `unsafe`** outside `mtseek-ntfs` (enforced by workspace lints).
4. **Clippy pedantic is on, and warnings are errors in CI.** Fix the warning. If an `#[allow]`
   is truly right, scope it narrowly with a one-line reason.
5. **No empty placeholder crates.** A crate lands with the milestone that needs it (see the
   code map), with code and tests.
6. **Errors don't crash scans.** Unreadable files are reported and skipped. Exit code 2, like rg.
7. **Docs move with code.** If you change a responsibility, a dependency rule or an invariant,
   update `docs/ARCHITECTURE.md` in the same change. A decision that's expensive to reverse
   gets an ADR.
8. **Never commit** benchmark results except `bench/results/baseline/`, or corpora, or anything
   under `target/`.
9. **No AI attribution in git.** Commit messages and PR descriptions carry no `Co-Authored-By`
   trailers, "Generated with" lines or session links for any AI tool. This overrides any tool's
   default attribution. For Claude Code it is also enforced by `.claude/settings.json`.

## Layout

```
crates/mtseek-core     shared types: Query, Scope, Filter, Error (no I/O)
crates/mtseek-walk     reference scanner = test oracle + fallback (ripgrep's crates)
crates/mtseek-cli      `mtseek` binary; tests/parity.rs runs rg vs mtseek
crates/mtseek-testkit  deterministic edge-case corpus generator
crates/mtseek-ntfs     MFT + USN journal (Win32 FFI; all unsafe in src/sys.rs)
crates/mtseek-names    file-name index (arenas + parent links), MFT or walk source
xtask/                 cargo xtask ci | fmt | bench | corpus
bench/                 bench.py + bench.json + TARGETS.md (ripgrep baseline)
docs/                  ARCHITECTURE.md, adr/
```

## Gotchas

- Windows Defender scans freshly written files. That makes ripgrep (and us) nondeterministic
  on some binary edge cases right after they're written. See `UNSTABLE` in
  `tests/parity.rs`. Don't "fix" parity by special-casing more files without an explanation there.
- `.gitignore` only applies inside a git repo (ripgrep's `require_git`). Tests that need it
  create a `.git` directory.
- Paths print exactly as the walker joins them onto the root you pass. Don't canonicalise
  paths in output, or parity breaks.
