---
status: open
priority: P2
related_adrs:
  - ADR-0011
  - ADR-0015
created: 2026-08-24
updated: 2026-08-24
---
# Issue 0052: Avoid durable write probes in frequent admin polling

- Status: Open
- Priority: P2
- Related ADR: ADR-0011, ADR-0015

## Objective

Keep the frequently polled management status endpoint read-only while preserving a
separate, meaningful archive-readiness check.

## Problem

The administration UI polls `/api/admin/v1/status` every three seconds. Each request
calls `Archive::check_readiness`, which creates, writes, synchronizes, removes, and
synchronizes a probe under `/data/tmp`. An open browser therefore causes continuous
durable writes and filesystem flushes on the QNAP archive volume even when ModelKeep
is otherwise idle.

## Scope

- Separate cached/current service status from the active write probe used by
  `/readyz` and the container health check.
- Ensure ordinary management-page polling does not create or remove archive files.
- Preserve detection of an unwritable archive through a bounded readiness mechanism.
- Document the freshness and meaning of the status value exposed to operators.

## Acceptance criteria

- Repeated `/api/admin/v1/status` requests perform no writes under the archive root.
- `/readyz` still reports an unavailable or unwritable archive within a documented
  interval.
- The UI continues to distinguish ready and not-ready service state.
- Tests count or intercept filesystem mutations so the polling regression is caught.

## Verification

```sh
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
nix flake check
```

Include a repeated-status test and a readiness failure/recovery test.

## Risks and assumptions

A directory-existence check alone is not sufficient readiness evidence. Any cache or
background probe must avoid hiding a newly read-only, full, or disconnected mount for
an unbounded period.
