# 0011: Keeping the content index current from the USN journal

**Status:** Accepted, 2026-11-21. Completes the lifecycle sketched in
[0010](0010-content-index-format.md) (M2b).

## Context

A content index built once (M2a) goes stale with the first edit, which breaks invariant 3: a
file saved before a query starts is searched as saved. The name index solved the same problem
by replaying the NTFS change journal (ADR 0008). The content index has three extra
difficulties:

- **Records carry no names.** Without admin rights a journal record has only the changed
  file's reference number (FRN), its parent folder's FRN and the reasons. Mapping a record to
  a path needs FRNs that the index remembers.
- **The file set follows ripgrep's filters.** A new `.gitignore`, a new `.git` folder or a
  hidden attribute changes which files belong to the scope. `.gitignore` itself is hidden,
  so it is not an indexed file.
- **Rewriting the segment is too slow for every query.** Rewriting it costs about as much as
  building it (1.75 s on `dev`).

## Decision

### What the index remembers (segment format v2)

- **Each file's FRN.** It is read from the handle the build opens anyway, so it costs no
  extra open.
- **Every folder the walker entered:** its FRN and path.
- **Every control file in those folders:** `.gitignore`, `.ignore`, `.rgignore` and `.git`,
  with FRN and path. They come from one FRN-carrying listing per folder.
- **One journal checkpoint per drive** (journal id, next USN), taken *before* walking, so
  anything that changes during the build is replayed.

### Catch-up, at the start of every search

1. **Read the journal from the checkpoint.** If there are no records, the index is current.
   That is one `FSCTL` call, a few milliseconds.
2. **Map each record through the FRN tables:**
   - a record about an indexed file marks that file changed, and its folder dirty
   - a record whose parent is an indexed folder marks that folder dirty (creates, deletes,
     renames, attribute changes)
   - a record about a control file marks its folder's whole subtree for re-walking
   - anything else is outside the scope, or under an ignored folder the walker never entered
3. **Re-walk** each such subtree with the same `ignore` walker: drop everything under it,
   then add what the walk finds. The walker reads the ignore files of every ancestor, so a
   subtree walk lists exactly what a full walk would list there. The catch-up test checks
   this against fresh builds.
4. **Re-list each dirty folder**, parents first. Its included entries come from the same
   walker limited to depth 1, and the FRNs come from an NTFS directory listing. Then compare
   with the index:
   - a file that is new, has a different FRN (a save by rename) or is marked changed is
     re-read
   - an indexed file that is gone, or now excluded, is dropped
   - a new subfolder, or one with a different FRN, is walked
   - a vanished subfolder's subtree is dropped
   - a control file that appeared, vanished or changed turns the re-listing into a subtree
     re-walk
   - a folder that can no longer be listed is dropped, and its parent is re-listed
5. **Commit.** Write a new delta segment holding:
   - every file added or re-read since the base was built
   - tombstones for the base files they replace or delete
   - the complete folder, control and error tables
   - the new checkpoints

   It is written to a temporary file and renamed into place, like the base. The new
   checkpoint is the journal position read *before* any file was re-read, so a change made
   during catch-up is replayed next time.

### Base plus delta

- The base never changes. A delta carries the base's build id and is ignored unless it
  matches.
- Queries run the trigram plan on both layers, skip the base's tombstoned files, and verify
  candidates from both.
- When the delta holds more than 1/8 of the base's files (at least 2,000), catch-up rebuilds
  the base instead. That bounds both the delta's size and the cost of rewriting it.
- The index is rebuilt when:
  - the journal was recreated, or has dropped records past the checkpoint
  - a root was deleted
  - the scope follows symlinks (`-L`): targets can be anywhere, so FRN tables can't cover
    them

### When the index answers

- `mtseek --index` builds an index for a scope that has none. Building stays opt-in, so a
  search of `C:\` never quietly writes gigabytes.
- **Once a scope has an index, every content search of that scope uses it**, caught up
  first. `--no-index` scans instead, and `--reindex` rebuilds.
- An index without journal checkpoints (not NTFS) can't be caught up. Without `--index` it
  is ignored and mtseek scans. With `--index` it is rebuilt.

## Consequences

- Invariant 3 holds for indexed scopes, with one documented gap, listed below. The catch-up
  test enforces it: random sequences of changes are applied to a folder tree (writes,
  appends, deletes, renames of files and folders, new folders, `.gitignore` edits, `.git`
  creation, the hidden attribute), and the caught-up index must equal a fresh build. That
  means the same files with the same bytes, folders and control files.
- **Known gap:** ignore rules that live outside the walked folders change nothing in the
  index until `--reindex`. These are `.git/info/exclude`, git's global excludes file, and
  ignore files above the scope root. The walker never enters `.git`, and the others are
  outside the scope.
- A search with no changes since the last one pays one journal read on top of M2a's cost.
  A search after changes also pays for re-reading the changed files and rewriting the
  delta. That cost is proportional to the delta, which is capped.
- Checkpoints advance only when a delta is written. Irrelevant records (changes elsewhere on
  the drive) are re-read by later searches until the next write. When more than 20,000 have
  piled up, the delta is rewritten just to move the checkpoint.
