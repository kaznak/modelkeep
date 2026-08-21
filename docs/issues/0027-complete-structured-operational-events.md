---
status: open
priority: P2
related_adrs:
  - ADR-0008
  - ADR-0009
created: 2026-08-21
updated: 2026-08-21
---
# Issue 0027: Complete structured operational events

- Status: Open
- Priority: P2
- Related ADR: ADR-0008, ADR-0009

## Objective

Emit consistent structured events for every minimum operational event named in the
development plan.

## Problem

JSON logging exists for requests, fetches, publication, and selected failures, but
event coverage and stable fields are not yet tested for hit/miss, verification,
recovery, and storage failure paths.

## Scope

- Define event names and credential-safe fields.
- Cover request, hit, miss, fetch lifecycle, verify failure, publish, disk-full, and
  incomplete recovery.
- Add log-capture tests without turning log format into durable archive state.

## Acceptance criteria

- Operators can distinguish each required event and correlate repository/revision.
- Tokens, signed URLs, and sensitive headers are absent from captured events.

## Verification

```sh
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
nix flake check
```

Include log-capture assertions for every required event and sensitive-field absence.

## Risks and assumptions

Storage capacity metrics remain in Issue 0004; this issue covers structured events,
not a Prometheus subsystem.
