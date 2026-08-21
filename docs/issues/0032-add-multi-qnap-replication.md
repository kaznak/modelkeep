---
status: open
priority: P3
related_adrs:
  - ADR-0001
  - ADR-0002
created: 2026-08-21
updated: 2026-08-21
---
# Issue 0032: Add multi-QNAP replication

- Status: Open
- Priority: P3
- Related ADR: ADR-0001, ADR-0002

## Objective

Replicate completed immutable archives between QNAP systems without upstream redownload.

## Problem

Backup is documented, but there is no online multi-NAS replication protocol or
conflict policy.

## Scope

Define complete-revision transfer, verification, ref convergence, resumability, and
failure isolation without exposing partial data.

## Acceptance criteria

Replicas contain ordinary independently serveable files and converge without
overwriting immutable revisions.

## Verification

```sh
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
nix flake check
```

Run interrupted transfer, conflicting refs, offline serving, and integrity scenarios.

## Risks and assumptions

Do not make a remote index or peer mandatory for local archive recovery.
