---
status: open
priority: P1
related_adrs:
  - ADR-0015
created: 2026-08-24
updated: 2026-08-24
---
# Issue 0054: Harden durable management job identity and idempotency

- Status: Open
- Priority: P1
- Related ADR: ADR-0015

## Objective

Give every persisted management job a collision-resistant identity and bind
idempotency replay to the authenticated request that created it.

## Problem

Job IDs combine whole-second wall-clock time with a process-local sequence that resets
on restart. A restart, clock rollback, or repeated process start within one second can
reuse an existing ID and overwrite its JSON record. In addition, idempotency stores
only a hash of the caller-supplied key; reusing a key with a different operation,
target, or principal silently returns the unrelated existing job.

## Scope

- Generate identifiers that remain unique across process restarts and wall-clock
  changes without relying on a database as archive authority.
- Persist records with collision-safe create/publish behavior.
- Bind idempotency to the normalized request and relevant principal scope.
- Return a clear conflict when a key is replayed with different request semantics.
- Preserve stable ordering and cursor pagination independently of identifier text.

## Acceptance criteria

- Restart and fixed/rolled-back-clock tests cannot overwrite an existing job record.
- Exact retries return the original job without starting duplicate work.
- Reusing a key for another kind, repository, revision, or principal returns a
  conflict and never leaks the prior job.
- Pagination neither duplicates nor skips jobs created in the same timestamp window.

## Verification

```sh
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
nix flake check
```

Add restart, collision injection, cross-request replay, cross-principal replay, and
same-timestamp pagination tests.

## Risks and assumptions

Existing job records need a backward-compatible reader or an explicitly documented
metadata-only migration. Model archive bytes and manifests must remain untouched.
