# 0012: What the daemon indexes: coverage roots, narrowed at query time

**Status:** Accepted, 2026-11-21. Changes how [0010](0010-content-index-format.md) and
[0011](0011-live-content-index.md) decide which files an index holds. First decision of M3.

## Context

In M2 a content index belongs to one **scope**: the exact roots and filter flags of the
search that built it (`scope_key`). A search uses an index only when its scope is identical.
That was right for a CLI without a daemon, but it doesn't work for one:

- **Narrower searches miss.** An index of `C:\Dev` doesn't serve `mtseek x C:\Dev\api`, or
  `C:\dev` (different case), or a root given by its 8.3 short name. Each would build another
  index of files already indexed.
- **Indexes multiply.** Each combination of roots and `-u` level gets its own index holding
  the same files. A machine-wide engine needs a bounded number of indexes it can keep in
  memory and up to date.
- **Ignore rules that belong to a user don't fit a shared index.** Git's global excludes
  file (`core.excludesFile`) is per-user configuration that ripgrep honours. A SYSTEM service
  serving several users can't bake one user's file into a shared index.
- **Known gap from 0011.** `.git/info/exclude` and ignore files above the root change nothing
  until `--reindex`.

Some ways of answering this break invariant 2 (index = oracle), so the decision has to keep
the index *exact* for every scope it claims to answer.

How ripgrep decides which files a search covers (`ignore` crate, as `mtseek-walk` configures
it):

- **Filter levels.** Level 0 (default) honours `.gitignore`, `.ignore`, `.rgignore`,
  `.git/info/exclude` and git's global excludes, and skips hidden entries. Level 1 (`-u`)
  stops honouring ignore files. Level 2 (`-uu`) includes hidden entries. Level 3 (`-uuu`)
  searches binary files in full instead of stopping at the first NUL. The file sets nest:
  level 0 ⊆ 1 ⊆ 2 = 3.
- **Parents are read.** A walk from root `R` reads the ignore files of every ancestor of `R`
  as well as those below it.
- **Roots are always entered**, even when the root itself is hidden or ignored.
- **Precedence.** When several ignore sources disagree, the first that matches wins:
  `.ignore`/`.rgignore`, then `.gitignore`, then `.git/info/exclude`, then the global
  excludes file. A whitelist (`!pattern`) is a match too.

## Decision

### 1. Coverage roots replace scopes

The engine indexes a set of **coverage roots**. Each is a directory on an NTFS volume with
one **indexed level** (0, 1 or 2).

- Coverage roots are **disjoint**: none is inside another.
  - Registering a root inside a covered one at the same or a lower level does nothing.
  - At a higher level, the request is refused with a message saying so. Raising the level
    of a whole subtree is a deliberate choice: level 2 on `C:\` would index every
    `node_modules`.
- **One segment set per volume** holds every coverage root on that volume, with the base
  and delta of 0011.
- **The index holds the files of the walk from each coverage root at its indexed level.**
  The file set, catch-up and the stored-bytes rule (cut at the first NUL unless UTF-16) are
  unchanged.
- **What the format adds:**
  - **For each file, one bit for each level below the root's indexed level:** whether the
    walk at that level also includes the file. Level-0 and level-1 searches of a level-2 root
    are then exact without a second index. Computing a bit means running the reference walker
    at that level. That pass only lists folders; each file is still read once.
  - **For each folder, the lowest level at which the walk enters it.**
- **Binary files, level 3.** A file whose stored bytes were cut at a NUL carries a flag.
  `-uuu` searches read flagged files from disk; every other file comes from the store. So
  level 3 never needs its own index.

### 2. How a search uses the index

A content search has roots `R₁…Rₙ` and a filter at level `k`. Each root is resolved on its
own:

1. **Find the folder.** Open `R`, read its file reference number and find the folder in the
   index by that number, not by spelling. That handles case, 8.3 names and junctions in the
   path.
2. **Fast path.** It applies when `R` lies inside coverage root `C`, the walk from `C` enters
   `R` at level `k`, and `k` ≤ `C`'s indexed level. The result is the indexed files under `R`
   whose bit for level `k` is set, verified from the store, with paths re-spelled onto `R` as
   typed. Exact because of the **subtree rule:**

   > If the walk from `C` at level `k` enters folder `R`, then the walk from `R` at level `k`
   > lists exactly the files that the walk from `C` lists under `R`.

   It holds because a walk from `R` reads the same ignore files: the parents of `R` include
   everything between `C` and `R`. It is a precondition, not an assumption. A property test
   in `mtseek-walk` checks it on random trees with nested `.gitignore`, `.ignore`,
   `.rgignore`, `.git` and hidden entries, for every entered folder and every level. Any
   construct that breaks it is excluded from the fast path.
3. **Walker path.** It applies when `k` is above `C`'s indexed level, or `R` was not entered
   at level `k`. Example: a search of `C:\Dev\api\node_modules`, which a level-0 walk never
   enters. The reference walker lists `R` at level `k`:
   - a file that is in the index (and current) is verified from the store
   - every other file is read from disk

   The walker decides the file set, so this path is exact by construction. Compared with a
   plain scan, it saves opening every file the index already holds. The cost is listing
   folders.
4. **Plain scan.** It applies when `R` isn't under any coverage root, is a file (ripgrep
   searches an explicit file whatever the filters), is on a volume that isn't NTFS, or the
   search follows links (`-L`). That is today's `--no-index` behaviour.

With several roots, each answers separately, as ripgrep does. Overlapping roots print a file
once per root, like `rg`.

### 3. Ignore sources outside the index's walk

- **Ignore files above a coverage root** are tracked like the control files of 0011. They
  are recorded with their file reference numbers, and a change to one re-walks the root.
- **`.git/info/exclude`** becomes a control file of its repository's root folder. A change
  re-walks that repository's subtree. That closes the known gap of 0011.
- **Git's global excludes file is applied at query time, per caller.**
  - The index computes level-0 bits with the global layer turned off (`git_global(false)`).
  - A global pattern can only *remove* files: its whitelists only cancel its own ignores.
    So the true set is the indexed set minus the files the global file ignores. A file is
    removed only when every higher-precedence source says nothing about it.
  - At query time, only the **matching** files are checked against the caller's global
    file. They are few.
  - The global file can ignore a matching file or a folder on its path; ignoring a folder
    prunes everything inside it. In that case a function in `mtseek-walk` works out what the
    higher-precedence sources say about the file and about each folder on its path. It uses
    the `ignore` crate's public `Gitignore` matchers, in ripgrep's precedence order, from the
    control files on that path. The file is dropped if the global file decides for it or for
    any of those folders. It lives next to the walker so
    the two can't drift apart, and the oracle test runs with random global excludes files.
  - In the service, the caller's global excludes file is read while impersonating the
    caller. It is cached per user and watched like a control file.

### 4. Secret exclusions keep their parity

The default secret exclusions of 0004 (`.env*`, `*.pem`, `*.key`, `id_rsa*`, vaults and so
on) keep files **out of the stored index**, not out of results. A search whose scope includes
an excluded file reads that file from disk, on the walker path, under the caller's identity.
Secrets never reach the index on disk, and results still equal `rg`'s. This sharpens 0004
rather than replacing it.

### 5. What gets covered by default

- In the CLI's direct mode, `mtseek --index <dir>` registers `<dir>` as a coverage root, at
  the level the search uses. That replaces the per-scope index of 0011.
- In the daemon, the default coverage is the roots users have registered plus each user's
  profile folder, at level 0.
- **Whole-volume coverage (`C:\` at level 0) stays opt-in** until it is measured: index
  size, build time, catch-up load and memory, recorded in `bench/TARGETS.md`. Making it the
  default is a separate, measured change.

### 6. Invariants and tests

| Test | Where | What it shows |
|---|---|---|
| Subtree rule | `mtseek-walk` property test | the precondition for the fast path |
| Oracle, extended | `mtseek-query/tests/oracle.rs` | search roots are random subfolders of a coverage root, with levels both at most and above the indexed level, plus random global excludes. Results equal the scanner's |
| Parity | `mtseek-cli/tests/parity.rs` | every case also runs as a subfolder search of an indexed parent |
| Spelling | `mtseek-cli` | roots given in another case and by 8.3 name print as typed (the 8.3 bug fixed in PR #1) |
| Catch-up | `mtseek-index/tests/live.rs` | also mutates `.git/info/exclude` and ignore files above the root; level bits must equal a fresh build's |

## Alternatives considered

- **Keep one index per scope.** The status quo: duplicate indexes, misses for every narrower
  search, and no bound on how many indexes exist. Rejected.
- **Index everything (level 3, whole volume) and filter at query time.** Binary files in full
  and every `node_modules`: on `C:\Dev` alone that is 1M files and 43.6 GB on disk, against
  15k files and 693 MB at level 0. Rejected on size and build time.
- **Index the widest set and evaluate ignore rules for every candidate at query time.** That
  re-implements ripgrep's walk outside the walker for every file, so broad searches pay per
  file and the logic can drift. Rejected, except for the global-excludes case above, which
  touches only matching files and is enforced by the oracle.
- **Nested coverage roots with per-subtree levels.** The level bits would then depend on which
  root they are relative to, which makes the fast-path rule hard to get exactly right.
  Deferred: if users need `C:\` at level 0 and `C:\Dev` at level 2, a later ADR can add it.

## Consequences

- One index per volume, bounded by the coverage policy. Subfolder searches, other spellings
  of a root and lower filter levels hit the index. Today all of them miss.
- Searches above the indexed level, or into folders the walk skipped, still pay for listing
  folders, but not for opening indexed files. The benchmark gains a subfolder search and a
  `-u` search over a level-0 root to measure this.
- Format v3: level bits per file, entry level per folder, the truncated-binary flag, the
  coverage table and the extra control files. Indexes are caches, so v2 indexes are ignored
  and rebuilt.
- `scope_key` and `segment_path` are replaced by a coverage table per volume. The CLI's
  direct mode uses the same resolution as the daemon, so both give the same answers. That is
  the first piece of M3a and works before the daemon exists.
- When ripgrep adds behaviour, it has to fit this scheme. When mtseek gains `-g` and `-t`:
  - exclusions and file types are path predicates applied to indexed files
  - a whitelist glob can re-include ignored or hidden files, so it takes the walker path
    unless the root is indexed at a level where nothing is excluded
