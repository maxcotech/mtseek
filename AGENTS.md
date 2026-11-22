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
| Index = scanner test | `cargo test -p mtseek-query --test oracle` |
| Caught-up index = fresh build | `cargo test -p mtseek-index --test live` (Windows) |
| Subtree rule (ADR 0012) | `cargo test -p mtseek-walk --test subtree` |
| Format | `cargo xtask fmt` |
| Benchmark vs ripgrep | `cargo xtask bench --queries rare,absent --modes files` |
| Generate a test corpus | `cargo xtask corpus <empty-dir>` |
| Run the CLI | `cargo run -p mtseek-cli -- -F needle C:\some\dir` |
| Run the daemon | `cargo run -p mtseek-daemon -- --console` (then `mtseek` answers through it) |
| Daemon = direct mode | `cargo test -p mtseek-cli --test daemon` (Windows) |
| What a caller may see (ADR 0014) | `cargo test -p mtseek-cli --test access` (Windows) |
| Service end to end, two accounts | `cargo test -p mtseek-daemon --test service` (elevated; skips otherwise) |

The toolchain is pinned in `rust-toolchain.toml`. Nothing needs admin rights except the MFT
enumeration test in `mtseek-ntfs` and the service test in `mtseek-daemon` (it installs a test
service and creates a local account), which skip when not elevated (CI is elevated and sets
`MTSEEK_REQUIRE_ADMIN=1`). To run them locally, run `cargo test -p mtseek-ntfs` and
`cargo test -p mtseek-daemon --test service` from an elevated terminal. The USN journal and
access-check tests run as any user.

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
crates/mtseek-cli      `mtseek` binary; tests/parity.rs runs rg vs mtseek (direct and daemon)
crates/mtseek-engine   what answers a search, shared by direct mode and the daemon
crates/mtseek-ipc      daemon protocol: JSON-RPC messages, framing, client
crates/mtseek-daemon   `mtseekd`: resident engine behind a named pipe
crates/mtseek-testkit  deterministic edge-case corpus generator
crates/mtseek-ntfs     MFT + USN journal + pipe, tokens, ACLs, SCM (Win32 FFI; all unsafe in src/sys.rs)
crates/mtseek-names    file-name index (arenas + parent links), MFT or walk source
crates/mtseek-index    content index: segments (postings + zstd store), live catch-up; tests/live.rs
crates/mtseek-query    regex -> trigram plan, candidate lookup, verification; tests/oracle.rs
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
- `mtseek --names` and `mtseek --index` cache their indexes in `%LOCALAPPDATA%\mtseek\cache`
  (`content-coverage.txt` lists the folders with a content index).
  Tests must set `MTSEEK_CACHE_DIR` to a temp dir (see `tests/names.rs`, `tests/parity.rs`)
  so they never touch the real cache.
- When `mtseekd` runs, `mtseek` answers through it, from the daemon's cache. CLI tests go
  through `tests/common/mod.rs` (`How`), which sets `MTSEEK_PIPE` either to a pipe nobody
  serves (direct mode) or to a daemon started for the test, so a daemon you are running
  never answers a test. With `MTSEEK_PIPE` set, the CLI tries no service pipe unless
  `MTSEEK_SERVICE_PIPE` names one, so an installed service never answers a test either.
- Every search checks what its caller may see against the security descriptors in the index
  (ADR 0014), in direct mode too. A permission change only updates the descriptor; the bytes
  stay. So a test that takes access away from itself (`security::set_dacl`, see
  `tests/access.rs`) must give it back before the temp dir is deleted.
- The content index must never change *what* matches, only how fast. If the oracle test
  fails, the index is wrong. Fix the planner or the stored bytes, never the test.
- Paths print exactly as the walker joins them onto the root you pass. Don't canonicalise
  paths in output, or parity breaks.
