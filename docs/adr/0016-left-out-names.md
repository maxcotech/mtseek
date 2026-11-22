# 0016: The index records what its walk leaves out, and the `--no-ignore-vcs` walks

**Status:** Proposed, 2026-11-22. Second decision of M4. Amends
[0010](0010-content-index-format.md) (segment format v5), [0011](0011-live-content-index.md)
(what catch-up keeps current) and [0012](0012-index-coverage.md) (which searches an index
answers). Follows [0015](0015-agents-json-and-mcp.md) §2.

## Context

ripgrep checks globs and types before ignore files and hidden names (0015 §2, measured with
ripgrep 15):

- a file that matches `-g` without `!` is searched even if it is ignored or hidden;
- a file of a `-t` type is searched even if it is hidden (but not if it is ignored).

Both apply in every folder the walk enters. An index built with ripgrep's default filters
holds neither kind of file, so since M4a it answers a glob or `-t` search only if it was
built with `-uu`, and scans otherwise. `-g '*.rs'` and `-t py` are the commonest searches
agents make, and most indexes are built with the defaults.

Separately, users want to search what `.gitignore` hides while `.ignore` files still apply
(`--no-ignore-vcs`). Since M4a that flag, its siblings and `--ignore-file` scan, because the
index records only walks that read all ignore files or none.

Measured on the `dev` benchmark tree (`C:\Dev`: 15,431 files in 2,895 folders the default
walk enters):

- Those folders hold **116 files and 160 folders the walk leaves out**: dotfiles
  (`.gitignore`, `.env`, `.nvmrc`, `.gitattributes`), some ignored `.json` and `.ts` files,
  and folders such as `.git`, `node_modules`, `bin`, `obj`, `.github` and `dist`.
- Building an index already reads the full listing of every folder it enters
  (`ntfs::list_dir`, for file reference numbers and control files), and catch-up re-reads
  the listing of every folder the journal marks dirty. The left-out names are in those
  listings; recording them reads nothing more from disk.
- The document table already stores flags in 32 bits; only six of the eight in-memory bits
  are used.

## Decision

### 1. Record left-out names

In every folder the index's walk enters, the entries of the folder's listing that the walk
leaves out are recorded:

- **A left-out file** becomes a document with no walk bits, marked `UNSTORED`: never read,
  never stored, with its file reference number and security descriptor. A search that lists
  it reads it from disk, as for any unstored file.
- **A left-out folder** becomes a directory entry with no walk bits. It is not entered, so
  nothing inside it is recorded.

Build takes them from the listing it already reads. Catch-up takes them from the re-listing
of a dirty folder, so a left-out file created, renamed or deleted is current at the next
search, like any other file.

### 2. What they answer

- **A glob without `!`** lists a left-out file in a folder the search enters: it is in the
  index now, so the search reads it from disk. `unanswerable` no longer needs a `-uu`
  index for globs. A glob that names a left-out folder still scans (the walk would enter a
  folder whose contents the index doesn't hold); the recorded folder names now let the index
  see that.
- **A `-t` type** lists a left-out file only if it was left out for being hidden, not for
  being ignored. The index doesn't record why. For the few left-out files of a selected type,
  the walker itself decides at search time: it lists their folder, one level deep, with the
  search's filter and hidden files on, as the scanner would (the subtree rule of 0012 makes
  that walk agree with the index's). On the `dev` tree that is at most a few folders per
  search.
- **Everything else is unchanged.** A search without rules never lists a document without
  walk bits.

### 3. The `--no-ignore-vcs` walks

Two more walk bits: `.ignore` and `.rgignore` honoured, `.gitignore`, `.git/info/exclude`
and git's global excludes not, with hidden entries skipped or included. Like the four
existing walks, they are recorded only in indexes whose coverage lists everything they list:
indexes built with `-u` or `-uu` (a default index never entered the ignored folders these
walks enter). Recording them costs two more listing walks at build and at catch-up, no reads.

`--no-ignore-vcs` then uses such an index, and the pipe protocol carries it (version 3).
`--no-ignore-dot`, `--no-ignore-parent`, `--no-ignore-global`, `--no-ignore-exclude` and
`--ignore-file` keep scanning in the client: each is another walk, and none is common enough
to pay for.

### 4. Format and compatibility

- **Segment format v5.** Flags become 16 bits in memory (`membership` gains `NO_VCS` and
  `NO_VCS_HIDDEN`); the on-disk field is already 32 bits. An index of an older version
  doesn't match its key and is rebuilt on first use, as at every format change.
- **The service** declines a search that reaches an unstored file the caller may read
  (0014 §6), so glob searches that list a left-out file are searched in the caller's own
  process. Searches that reach none are answered as before.
- **No new disk reads at build or catch-up** for left-out names; two listing walks more for
  `-u` and `-uu` indexes.

### 5. Tests and measurement

- **Oracle:** random corpora already put files in hidden and ignored places and random
  globs and types on searches (M4a). The test then requires the index to answer every glob
  and type case except a glob that names a left-out folder and the `!`-glob error cases of
  0015, instead of accepting any decline. `--no-ignore-vcs` joins the filters it searches.
- **Live:** random changes that create, rename and delete ignored and hidden files, and
  folders, in entered folders; after each, the caught-up index (left-out entries and the new
  walk bits included) must equal a fresh build.
- **Parity:** the M4a cases without the `-uu` round, which existed only to give globs an
  index, and `--no-ignore-vcs` through `--index`.
- **Benchmark:** `-g '*.rs'`, `-g '*.log'`, `-t ts` and `--no-ignore-vcs` cells on the `dev`
  scope, before and after, and the build time and size of the `dev` index.

## Alternatives considered

- **Index everything (`-uu`) by default.** Answers every search, but the `dev` tree's ignored
  folders (`node_modules`, `bin`, `obj`, `dist`) hold most of its bytes, which the index
  would read, store and keep current for searches that mostly don't want them.
- **List the entered folders at search time.** No format change, but it costs a listing per
  folder: 2,895 for `dev`, over 100,000 for a drive. That is the cost the index exists to
  avoid.
- **Record why each name was left out (ignored, hidden or both).** `-t` would need no
  search-time walk. But working out "ignored" outside the walker means applying ignore
  files ourselves: the precedence of `.gitignore`, `.ignore`, `.rgignore`, parents, git
  excludes and `require_git` that `mtseek-walk` gets from ripgrep's crates. A second
  implementation would drift; the search-time walk is the walker itself, for a few folders.
- **More walk bits for every `--no-ignore-*` flag.** Five flags and their hidden variants
  are ten walks to list at every build and catch-up, for flags rarely used. Only
  `--no-ignore-vcs`, the one users ask for, gets them.

## Consequences

- An index built with default filters answers `-g` and `-t` searches, the common agent
  searches, without `-uu`.
- An index built with `-u` or `-uu` answers `--no-ignore-vcs`.
- Every existing index is rebuilt once (format v5).
- Indexes grow by one document or directory entry per left-out name (276 on `dev`, against
  15,431 files).
- `mtseek-query` gains a search-time walk of a few folders, through `mtseek-walk`; it adds
  no ignore logic of its own.
- Pipe protocol version 3 for `--no-ignore-vcs`.
