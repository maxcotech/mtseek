# 0014: Service mode: one machine-wide daemon that shows each caller only what it could read

**Status:** Accepted, 2026-11-21. Built in M3c; [As built](#as-built-m3c) lists where the
build differs from the proposal. First decision of M3c. Makes
[0004](0004-service-security.md) concrete for the daemon of [0013](0013-daemon-and-ipc.md).
Sharpens 0004's access cache and supersedes two details of [0012](0012-index-coverage.md)
(§3's impersonation, §5's default coverage); see [Consequences](#consequences).

## Context

M3b runs `mtseekd --console` per user. Each console daemon serves only its own account, so
everything it indexed may be shown to its client. Service mode is the other half of 0013's
process model: one `mtseekd` per machine, running as LocalSystem, serving every local user.
0004 fixed the shape: a SYSTEM service, a pipe for interactive users, results checked
against the caller's access, the index readable only by SYSTEM and Administrators. This ADR
settles what 0004 left open and what is expensive to change later:

- how the service decides, exactly and cheaply, what a caller may see
- how it learns what only the caller knows: drive letters, the working directory, git's
  global excludes file
- the cache folder and the service's lifecycle
- what the service refuses to answer

**What ripgrep shows a caller.** ripgrep is the spec (0005), so "what the caller may see"
means "what `rg` run by the caller prints". `rg` lists each folder from the search root down,
which needs `FILE_LIST_DIRECTORY`. It opens each file it searches with `GENERIC_READ`. A folder it can't list or a file it can't open is an error line
(`Access is denied. (os error 5)`) and exit code 2. `rg --files` lists the names in a listable
folder without opening the files, so a name can be visible when its contents aren't. Folders
above the search root don't need to be listable: bypass-traverse checking is on for everyone
by default.

**What SYSTEM doesn't share with the caller:**
- drive mappings and `subst` drives (each logon session has its own DOS device map)
- environment variables, which decide where git's global excludes file is
- EFS keys
- the cloud-files sync provider, which runs in the user's session

**Measured** (spike in the session scratchpad, not committed; release build on this
project's reference laptop):

| | cost |
|---|---:|
| fetch a file's security descriptor by path (`GetFileSecurityW`: an open) | 42 µs (`C:\Dev`), 68 µs (profile), 127 µs (Program Files, Windows) |
| the same for all 15,422 files of `C:\Dev` at level 0 | 650 ms |
| `AccessCheck` on a descriptor with an identification token | 1.1–2.0 µs |
| look up a cached decision by descriptor | 0.06 µs |
| `GetSecurityInfo` on a handle already open to read the file | 7 µs (open + first 64 KiB read: ~47 µs) |
| distinct descriptors: 15,422 files in `C:\Dev` | **1** |
| distinct descriptors: 60,000 files in the profile (`-uu`) | **20** |
| distinct descriptors: 40,000 files in Program Files and Windows | **38** |

Checking each result by opening it is the cost the whole design avoids. A common query
matches thousands of files, so checking its results by opening each one would add
hundreds of milliseconds. Descriptors are very few, so a decision cached per descriptor is
nearly free.

## Decision

### 1. The service host

- **`mtseekd --install`** registers the service. It needs an elevated administrator.
  - The service is named `mtseek` and runs in its own process (`SERVICE_WIN32_OWN_PROCESS`)
    as LocalSystem.
  - It starts automatically at boot. After a failure the SCM restarts it twice, 5 s apart.
  - Its binary path is `"<this exe>" --service`.
- **`mtseekd --uninstall`** stops the service and deletes it.
- **`mtseekd --service`** is the SCM's entry point and is not meant to be run by hand.
  - It runs the same resident engine as console mode.
  - On stop, shutdown and preshutdown it writes the deltas held in memory, then exits.
- **The MSI (M6) registers the same `--service` entry**, so this is the only service code.
- **The dispatcher, the control handler and the SCM calls are FFI in `mtseek-ntfs/src/sys.rs`**,
  next to the pipe and token calls (invariant 6). That is about 150 lines, and a SYSTEM
  process gains no dependency to audit.
- **Console mode doesn't change.** A per-user console daemon still answers before the
  service (§3).

### 2. The cache folder and logs

- **Everything lives under `%ProgramData%\mtseek`:** `cache\` holds the indexes and the
  coverage registry, `logs\` holds the logs.
- **The folder is locked down:**
  - owner Administrators
  - a protected DACL `D:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)`, so no inherited entries
- **The service checks the folder at every start.** Any user can create folders in
  `%ProgramData%`, so another user could have created `mtseek` first and kept access to it.
  - A reparse point is refused.
  - A wrong owner or DACL is reset recursively.
  - If the reset fails, the service refuses to start and says why.
- **Logs:**
  - one line per request: method, caller SID, duration and counts
  - **never the query text or any path**, because what a user searches for can itself be
    sensitive
  - two files of at most 8 MiB, rotated
  - The SCM's own entries in the System event log already record starts, stops and crashes.
    An event source of mtseek's own needs a message DLL, so it waits for the MSI.

### 3. The pipe and the caller's identity

- **The pipe:** `\\.\pipe\mtseek` with DACL `D:P(D;;GA;;;NU)(A;;GA;;;SY)(A;;GA;;;BA)(A;;GRGW;;;IU)`.
  - Interactive users (console and remote desktop) can connect.
  - Network logons are denied, and `PIPE_REJECT_REMOTE_CLIENTS` turns away remote clients.
- **Squatting is handled on both sides:**
  - The service creates the first instance with `FILE_FLAG_FIRST_PIPE_INSTANCE`. If the name
    is taken, the service logs it and retries every 5 s.
  - Clients check that the server process runs as LocalSystem (`S-1-5-18`) before sending
    anything (0013).
- **Clients try the per-user pipe first, then the service pipe**, each with 0013's 50 ms
  timeout. If neither answers, they run in direct mode.
- **How the service identifies a caller.** For each connection it calls
  `ImpersonateNamedPipeClient`, then `OpenThreadToken(TOKEN_QUERY)`, then `RevertToSelf`. It
  keeps the token for the life of the connection.
  - Clients keep opening with `SECURITY_IDENTIFICATION`, so the token is good for access
    checks and group membership, but the service can never act as the caller.
  - The service never touches a file while impersonating.

### 4. What a caller may see

**Invariant 5, sharpened:** a caller sees exactly what `rg` run by that caller would show.
- Names: those in folders it may list, from the search root down.
- Lines: those from files it may read.
- Everything else: the same `Access is denied` error `rg` prints, with exit code 2.

An unreadable file whose name sits in a listable folder appears in `--names` output, as in
`rg --files`. Its contents never appear.

**The decision is `AccessCheck`** on the item's descriptor with the caller's token.
- The access masks are the ones `rg` asks for:
  - folders: `FILE_LIST_DIRECTORY | SYNCHRONIZE`
  - files: `FILE_GENERIC_READ`
- Using the token, not just the SID, means groups, deny entries, restricted tokens,
  AppContainers and integrity labels all count, as they would for `rg`.
- The stored descriptor includes the mandatory label, which `AccessCheck` needs for
  integrity checks.

**Descriptors are recorded in the index:**
- **Format v4:**
  - each segment has a table of distinct self-relative descriptors (owner, group, DACL,
    label)
  - each file entry carries a `u32` descriptor id (a document entry grows from 36 to 40
    bytes)
  - each folder entry carries one too
- **When building:**
  - Files: the builder reads the descriptor from the handle it reads the file with
    (+7 µs on ~47 µs).
  - Folders: one extra open each.
  - The service builds with `SeBackupPrivilege` and backup semantics, so no DACL keeps a file
    out of the index. Visibility then depends only on the caller's check, never on what
    SYSTEM happened to be allowed.
- **The name index** takes each entry's NTFS security id from the MFT pass. The USN
  record's `SecurityId` field is to be verified first; if it is empty, the id comes from
  `$STANDARD_INFORMATION` in the raw record (0009). Each distinct id is resolved to a
  descriptor once.

**Descriptors are kept current:**
- A journal record with `USN_REASON_SECURITY_CHANGE` marks the file or folder dirty, like a
  data change, and catch-up records its new descriptor.
- NTFS writes inherited ACEs into each child, so a permission change on a folder produces a
  record for every child it reaches.
- **Invariant 3, extended:** a permission change made before a search starts is honoured
  by that search.

**Decisions are cached per connection, per descriptor id.** A search does a few dozen
`AccessCheck` calls at most, then 0.06 µs per item.

**The visible set comes first.** Before planning, a search works out:
- which folders under its roots the caller may list, pruning beneath the ones it may not
- which files the caller may read

Postings are intersected with that set. Verification, `$/hit`, the summary, `--stats` and
`$/note` never see an invisible file, so neither counts nor timings in the output can
reveal one. Error lines for items the caller may not open are generated from the same
pass, worded as `rg` words them.

### 5. What only the caller knows: the client sends it

- **Roots.** For each root, the client opens it as itself, then sends the typed spelling
  plus the volume GUID and the file reference number.
  - The service finds the folder by its reference number (0012 §2, step 1). Mapped drives,
    `subst` drives, relative paths and 8.3 names then resolve in the caller's namespace,
    and results are re-spelled onto the root as typed, as today.
  - The client's open doubles as the access check on the root itself.
  - A root the client can't open goes to direct mode, which prints `rg`'s error.
- **Git's global excludes file.**
  - The client finds `core.excludesFile` as `rg` would, from its own environment and git
    config, and sends its path and bytes (at most 1 MiB).
  - The service applies it at query time to the matching files only (0012 §3).
  - This replaces 0012 §3's "read while impersonating the caller". An identification token
    can't read files, and the service doesn't have the caller's environment, which decides
    where the file is.
  - It also closes the gap direct mode has today: the index is built without the global
    layer, and every search applies its caller's file.
- **Both are optional fields of `search`**, so the protocol stays at version 1 (0013). The
  service declines a search that lacks them (§6). A console daemon ignores them.

### 6. What the service declines

**A new error code, `DECLINED` (-32003), means "not here, search it yourself".** The client
then runs in direct mode, silently, and `--stats` says why. Exactness never depends on the
service: whatever it doesn't answer exactly, it declines.

**Version 1 answers a search only when every root takes the fast path** (0012 §2, step 2)
on a volume the service covers. It declines:
- the walker path and plain scans: roots outside coverage, levels above the indexed level,
  folders the indexed walk skipped, files given as roots, and `-L`
- a root it can't resolve, or a request without §5's fields
- a scope that contains a file the service could not store but the caller may read:
  EFS-encrypted files, and cloud placeholders whose data isn't on disk. The indexer never
  reads a placeholder, so a background service never downloads a user's OneDrive.
- a scope whose ignore files include one the caller may not read, because the caller's
  `rg` would then apply different rules

The walker path could be served by checking each listed folder and unindexed file as it
goes (42–127 µs each). That waits for a measurement showing how often searches decline.

**Secret files** (0012 §4) are recorded with their path and descriptor but no contents. A
search that reaches one reads it from disk after the caller's check allows it. This is the
same check that guards every other result, and the index on disk never holds the bytes.

### 7. Coverage, status and shutdown with many users

- **Registering coverage.** `mtseek --index <dir>`, answered by the service, registers
  `<dir>` only if the caller may list it. A volume root needs an elevated administrator.
  The registry records who registered each root.
- **No default coverage in version 1.** 0012 §5 would index every user's profile by default.
  For the service, that would mean background indexing nobody asked for, and placeholder
  files on OneDrive-backed profiles. It waits for the measurements 0012 §5 already asks for.
- **`status`:**
  - shows the coverage roots the caller may list; elevated administrators see all of them
  - journal positions and sizes are per volume and name no paths
- **`shutdown`** is allowed only for an elevated administrator: `CheckTokenMembership` for
  Administrators, on the caller's token. Everyone else stops the service through the SCM
  (`sc stop mtseek`), which applies its own access check.
- **Fairness.** Searches still run one at a time (0013), so one user's slow search delays
  another's. That is accepted for version 1 and logged with durations so it can be
  measured.

### 8. Tests

| Test | Needs | What it shows |
|---|---|---|
| Visibility, unprivileged | any user | Build an index, then deny yourself read on matching files and list on a folder (the owner keeps `WRITE_DAC`, so cleanup can restore it). Search through the engine with your own token: output equals `rg`'s, with `Access is denied` lines and no lines from the bytes still stored. Counts in `--stats` match. |
| Visibility property test | any user | Random trees with random descriptors from a small SDDL set (allow and deny entries for the current SID, inherited and explicit). After catch-up, filtered results equal `rg` run as the same user. |
| Descriptor catch-up | any user | `live.rs` also changes ACLs: descriptor ids after catch-up equal a fresh build's. |
| Two accounts | elevated CI (`MTSEEK_REQUIRE_ADMIN`) | Installs the service under a test name and pipe, creates a second local account, and runs `mtseek` and `rg` as that account (`Start-Process -Credential`). Output equals `rg`'s, and nothing from the first account's private files appears (0004). |
| Cache folder | elevated CI | Create `%ProgramData%\<test name>` with an open DACL beforehand; the service resets it or refuses to start. |
| Protocol input | any user | Property test of the framing and message parser on random, truncated and oversized input. Other users' requests are now untrusted input (0004). cargo-fuzz targets follow when CI has a nightly job. |

## Alternatives considered

- **Impersonate the caller and open each result as them.**
  - It is exact by construction.
  - But each result costs an open (42–127 µs, 650 ms for every file of `C:\Dev`).
  - Clients would have to grant the service `SECURITY_IMPERSONATION`, letting any server on
    the pipe act as them.
  - Rejected for the search path.
- **Cache decisions per (user, folder), as 0004 sketched.**
  - A file can carry explicit entries that differ from its folder's.
  - Descriptor ids cost 4 bytes per file and are exact, and there are fewer distinct
    descriptors than folders.
  - Replaced.
- **One index per user inside the service.**
  - Content readable by several users would be stored several times.
  - It still needs the same check for files shared between users.
  - Rejected.
- **Hand the files the service can't read to the client** (a `$/delegate` list).
  - More searches would stay in the service.
  - But every client (the MCP server, the app) would need its own file searcher.
  - Deferred; it is an optional protocol addition if declines turn out to be common.
- **The `windows-service` crate.** A good crate, but the FFI is short and sits next to
  the pipe and token code that is already ours. One less dependency in a SYSTEM process.
  Rejected.
- **Default coverage of every profile.** See §7. Deferred until measured.

## Consequences

- **Index format v4:**
  - a descriptor table, plus a descriptor id per file and per folder
  - indexes are caches, so v3 indexes are rebuilt
  - console and direct mode store descriptors too, so there is one format
  - building pays about 7 µs per file and one open per folder; the benchmark measures the
    build before and after
- **Search cost** in the service:
  - one pass over the scope's files and folders to work out the visible set, about 1 ms for
    15,000 files (to be measured)
  - plus the console daemon's costs
- **The security-critical code sits in two places:**
  - the visible-set computation in `mtseek-engine`, used by every host with an
    "everything is visible" policy outside the service
  - the token, `AccessCheck`, ACL and SCM calls in `sys.rs`
  - A bug in either is a security bug.
- **ARCHITECTURE changes:**
  - Invariant 5 is reworded as in §4.
  - Invariant 3 covers permission changes.
  - The `mtseek-daemon` and `mtseek-ntfs` rows gain service mode.
- **Changes to earlier ADRs, recorded in each when this one is accepted:**
  - 0004's per-folder access cache becomes a per-descriptor cache, and secrets follow
    0012 §4.
  - 0012 §3: the client sends its global excludes file; the service doesn't read it while
    impersonating.
  - 0012 §5: the service has no default coverage until measured.
- **The protocol stays at version 1:** two optional `search` fields and one error code.
- **CI:**
  - The elevated job creates a local account and installs and removes a test service.
  - Locally, those tests skip unless the terminal is elevated, like the MFT test.
- **Searches the service declines** cost one round trip before direct mode. The
  benchmark measures that, and the log counts declines by reason.

## As built (M3c)

Where the build differs from the proposal above, and why:

- **Every host checks access, not only the service.** A permission change only updates the
  recorded descriptor; catch-up doesn't read the file again, so the index may keep bytes its
  builder can no longer read. Direct mode therefore checks with the process's own token,
  and a console daemon with its client's. In both, an unknown descriptor grants access,
  because the same account built the index. Only the service is strict. One code path
  serves every host, so the parity suite exercises it.
- **Roots: no volume GUID.** The service finds each root from the client's working directory,
  as a console daemon does. The client sends each root's volume serial number and file
  reference number, and the service declines if the same path names another folder for it.
  A root reached through a link, a junction or a `subst` drive is declined: walking from that
  spelling reads other ignore files above it than the index's walk did, so only direct mode
  is exact there. A drive mapped in the user's session doesn't exist for SYSTEM, so it is
  declined too.
- **Global excludes: sent, not yet applied.** The client sends the path and contents of its
  global excludes file. A search that honours ignore files is declined if that file holds any
  rule, so a caller with one gets direct mode. Applying the file to matching files (0012 §3)
  comes later. The service builds as SYSTEM, whose profile has no global excludes file.
- **Names through the service are declined.** That leaves direct mode, as before. Security
  ids from the MFT pass come with the names work.
- **`SeBackupPrivilege` isn't used yet.** The service reads files as SYSTEM, which every
  default DACL allows. A file SYSTEM may not read is a stored error, and callers see that
  error.
- **The pipe's DACL** gives interactive users read, write and attribute access (`0x12019b`)
  rather than generic read and write. Generic write includes `FILE_CREATE_PIPE_INSTANCE`,
  which would let any user serve on the name. Clients ask for exactly what they use. The
  pipe's owner (the process that created it) has full access, so a test daemon running as a
  user can create instances.
- **How a client checks the server is LocalSystem.** Only administrators may open a
  LocalSystem process, so an ordinary user can't read the server's token. Then the client
  reads the pipe's owner instead, which its `READ_CONTROL` allows. Every daemon sets itself
  as its pipe's owner, and Windows lets a process make only its own user, or a group it may
  own objects as, the owner: a squatter without administrator rights can't create a pipe
  owned by SYSTEM. The elevated two-account test found this; the in-process tests run as
  the server's own user, who may open its process.
- **The cache folder:** if its owner isn't SYSTEM or Administrators, it is deleted and
  created again with the right security. If the owner is right, the DACL is set again (not
  recursively: inside a folder only those two can write to, everything inherits).
- **Coverage registrants aren't recorded.** Registering checks that the caller may list the
  folder, and that a whole drive comes from an elevated administrator.
- **Logging:** plain timestamped lines, no `tracing`.
- **The two-account test** (`mtseek-daemon/tests/service.rs`) installs a test service,
  creates a local account and compares its `mtseek` with its `rg`. It runs only elevated, so
  in CI. It also checks that a cache folder someone else created first is made safe. Without
  admin, the same checks run against a service-mode daemon started in the test process
  (`mtseek-daemon` unit tests) and through `tests/access.rs`.
- **Two fixes found on the way:**
  - Files the index doesn't store (over 256 MiB, and now secret files) had no trigrams, so
    the planner never picked them as candidates, and searches through the index missed
    them. They are now candidates for every search.
  - Re-walking a `.git` or `.git\info` folder during catch-up dropped the control entries
    that the folder above owns. The catch-up property test found it once permission changes
    reached those folders.
- **Measured:** see `bench/TARGETS.md`, M3c.
