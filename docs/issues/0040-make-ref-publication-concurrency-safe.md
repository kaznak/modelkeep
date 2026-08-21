---
status: open
priority: P1
related_adrs:
  - ADR-0002
  - ADR-0012
created: 2026-08-21
updated: 2026-08-21
---
# Issue 0040: Make mutable ref publication concurrency-safe

- Status: Open
- Priority: P1
- Related ADR: ADR-0002, ADR-0012

## Objective

Allow concurrent processes to atomically update the same mutable ref without sharing
or losing a temporary file.

## Problem

Every update of a ref uses the same `.<ref>.part` path. Concurrent writers can
truncate the same inode, publish another writer's bytes, or fail because the shared
temporary name was already renamed.

## Acceptance criteria

- Each ref update uses an operation-unique temporary file.
- Every successful update publishes one complete commit ID atomically.
- Temporary files are cleaned after success and ordinary failure.
- A concurrent-update stress test produces only complete valid ref values.

## Verification

```sh
cargo test --all-features
nix flake check
```

## Risks and assumptions

Last successful writer wins. This issue does not add compare-and-swap semantics or
change explicit refresh policy.
