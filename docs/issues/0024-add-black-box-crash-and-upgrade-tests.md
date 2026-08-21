---
status: open
priority: P1
related_adrs:
  - ADR-0001
  - ADR-0008
  - ADR-0009
created: 2026-08-21
updated: 2026-08-21
---
# Issue 0024: Add black-box crash and upgrade tests

- Status: Open
- Priority: P1
- Related ADR: ADR-0001, ADR-0008, ADR-0009

## Objective

Prove process-level crash safety and archive compatibility across executable versions.

## Problem

Unit tests cover publication and recovery states, but no test SIGKILLs a live fetch,
and the restore drill uses the same executable before and after copying the archive.

## Scope

- Kill a live acquisition before publication and restart against the same archive.
- Prove no partial revision is served and recovery preserves unrelated complete data.
- Create an archive with a released/older executable and serve it with the new one
  without migration or redownload.

## Acceptance criteria

- Crash and cross-version scenarios run as black-box tests in CI.
- Published files remain byte-identical and incomplete data is never observable.

## Verification

```sh
nix develop -c cargo test --all-features
nix flake check
```

Run the focused SIGKILL and old-writer/new-reader black-box checks on amd64 and arm64.

## Risks and assumptions

Cross-version fixtures must remain reproducible and must not depend on mutable image
tags or upstream availability.
