# Issue 0004: Add storage observability

- Status: Open
- Priority: P1
- Related ADR: ADR-0004, ADR-0006

## Objective

Make archive capacity, disk-full conditions, and recovery activity visible to
operators before they affect model acquisition.

## Problem

QNAP storage is the durable state, but the current service does not expose archive
size, free space, or a clear capacity warning. Operators may discover a full volume
only after an acquisition fails.

## Scope

- Structured events for ENOSPC and relevant I/O failures.
- A read-only status or metrics surface for archive bytes and filesystem capacity.
- Preserve the rule that disk pressure never deletes completed revisions automatically.

## Acceptance criteria

- ENOSPC is distinguishable from an upstream failure.
- Existing completed revisions remain intact after a failed write.
- Operators can inspect archive size and available space without a database being the
  source of truth.
- Recovery of incomplete staging remains observable.

## Verification

```sh
cargo test --all-features
cargo clippy --all-targets --all-features -- -D warnings
```

Include a controlled low-space or injected-I/O-failure test where practical.

## Risks and assumptions

Filesystem statistics differ across container runtimes and mounted QNAP shares. The
first implementation should report clearly what filesystem or path it measures.
