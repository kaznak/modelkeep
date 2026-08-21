---
status: open
priority: P3
related_adrs:
  - ADR-0012
  - ADR-0013
created: 2026-08-21
updated: 2026-08-21
---
# Issue 0030: Add a management API

- Status: Open
- Priority: P3
- Related ADR: ADR-0012, ADR-0013

## Objective

Provide an optional authenticated API for existing explicit administrative operations.

## Problem

Refresh, import, remove, and audit are local CLI operations; remote automation has no
supported management surface.

## Scope

Define an authenticated control-plane boundary separate from download compatibility,
with idempotency and audit behavior. A web UI is not implied.

## Acceptance criteria

Ordinary download clients cannot invoke management operations, and all mutations
preserve existing archive invariants.

## Verification

```sh
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
nix flake check
```

Run black-box authorization, idempotency, crash-safety, and credential-leakage tests.

## Risks and assumptions

Do not expose local filesystem paths or weaken explicit deletion/refresh semantics.
