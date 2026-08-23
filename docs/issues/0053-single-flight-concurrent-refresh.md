---
status: open
priority: P1
related_adrs:
  - ADR-0002
  - ADR-0008
  - ADR-0012
created: 2026-08-24
updated: 2026-08-24
---
# Issue 0053: Single-flight concurrent refresh operations

- Status: Open
- Priority: P1
- Related ADR: ADR-0002, ADR-0008, ADR-0012

## Objective

Make concurrent refreshes of the same repository and mutable ref converge on one
acquisition and one consistent result.

## Problem

Cold-miss acquisition uses `SingleFlight`, but `PullThrough::refresh_with_progress`
does not. Two API or CLI refreshes for the same ref can download the same large
snapshot twice. When both resolve to the same new commit, one publication can win and
the other currently surfaces `AlreadyPublished` as a failed conflict instead of
converging on the verified completed revision.

## Scope

- Coordinate refresh operations at an appropriate repository/ref key.
- Reuse a concurrently published revision only after confirming it is complete.
- Preserve explicit ref-update semantics and dry-run behavior.
- Define progress/result behavior for joined management jobs without holding a global
  lock across network or large-file I/O.

## Acceptance criteria

- Concurrent refreshes of the same ref perform one upstream acquisition where
  practical and both return the same successful commit.
- A conflicting incomplete or corrupt publication is never accepted as convergence.
- Different repositories or refs are not serialized globally.
- Failed acquisition is propagated to all joined callers and a later retry can run.

## Verification

```sh
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
nix flake check
```

Add deterministic concurrent same-ref, different-ref, failure propagation, retry,
and incomplete-winner tests.

## Risks and assumptions

Mutable refs may advance between genuinely separate refresh operations. Coordination
must cover simultaneous work without permanently memoizing an upstream resolution.
