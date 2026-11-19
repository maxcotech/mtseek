# Architecture decision records

One file per decision that is expensive to reverse. Format: context → decision → consequences.
Never edit an accepted ADR's decision. Write a new ADR that supersedes it and link both ways.

| # | Decision | Status |
|---|---|---|
| [0001](0001-trigram-index.md) | Trigram index for content search | Accepted |
| [0002](0002-content-store.md) | Store compressed file text in the index | Accepted |
| [0003](0003-gpui-desktop-app.md) | GPUI for the desktop app | Accepted, pending spike |
| [0004](0004-service-security.md) | SYSTEM service that filters results by the caller's ACLs | Accepted |
| [0005](0005-ripgrep-compatible-contract.md) | ripgrep is the behavioural contract | Accepted |
| [0006](0006-windows-first.md) | Windows (NTFS) first | Accepted |
