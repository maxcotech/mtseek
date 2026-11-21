# 0004: SYSTEM service that filters results by the caller's ACLs

**Status:** Accepted, 2026-11-19. Made concrete by [0014](0014-service-mode.md): access is
decided per recorded security descriptor rather than cached per (user, folder), and secret
files follow [0012](0012-index-coverage.md) §4.

## Context

Reading the MFT and the USN journal needs administrator rights (a volume handle on `\\.\C:`).
So the engine runs as a Windows service under LocalSystem, which can read *every* file.
The index therefore holds content from files that some users on the machine must not see.
Many desktop search tools ignore this.

## Decision

- `mtseekd` runs as a service. Clients connect over a named pipe whose DACL admits
  interactive users and denies network logons.
- For each request the service calls `ImpersonateNamedPipeClient` and checks access to each
  result **as the caller** before returning it. Checks are cached per (user SID, directory) and
  invalidated by USN events.
- The index directory (`%ProgramData%\mtseek`) is ACL'd to SYSTEM + Administrators only.
- Default exclusions for secrets, which users can extend: `.env*`, `*.pfx`, `*.pem`, `*.key`,
  `id_rsa*`, browser profile directories, password-manager vaults.
- Everything that parses untrusted input gets a fuzz target: IPC messages, query parser,
  segment reader.
- **Admin-free mode.** Without the service, mtseek builds a per-user index under
  `%LOCALAPPDATA%` from the reference walker plus `ReadDirectoryChangesW`. Same features,
  slower first build, no elevated component at all.

## Consequences

- An access check per result costs time. Caching by directory keeps it off the fast path,
  and the benchmark measures it.
- A bug here is a security bug. ARCHITECTURE invariant 5 gets a dedicated integration test
  with two local accounts in CI.
