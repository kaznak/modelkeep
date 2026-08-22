---
status: open
priority: P2
related_adrs:
  - ADR-0004
  - ADR-0007
  - ADR-0013
created: 2026-08-22
updated: 2026-08-22
---
# Issue 0048: Add the remote revision deletion workflow

- Status: Open
- Priority: P2
- Related ADR: ADR-0004, ADR-0007, ADR-0013

## Objective

Expose ADR-0007 deletion through a separately authorized, deliberate API and UI
workflow without turning capacity pressure into automatic garbage collection.

## Problem

Remote deletion is materially riskier than inventory or prefetch. A generic mutation
endpoint or one-click UI action could destroy the only durable copy of a revision,
and browser retries or CSRF could repeat destructive requests.

## Scope

- Add a dry-run API returning the exact repository, commit, refs, and validation
  outcome without accepting arbitrary paths.
- Require a fresh confirmation bound to that dry-run result before deletion.
- Separate deletion capability from read and acquisition capabilities.
- Reject referenced revisions exactly as the CLI does and emit a durable audit event.
- Make retries idempotent and distinguish already-absent from newly-deleted state.
- Add an intentionally high-friction UI confirmation showing immutable commit ID.

## Acceptance criteria

- Read-only and acquisition-only administrators cannot delete revisions.
- A referenced revision cannot be deleted through API, UI, retry, or forged request.
- A stale or altered confirmation cannot delete a different revision.
- Successful deletion affects only the reviewed unreferenced commit and is auditable.
- No automatic GC, bulk-delete default, path deletion, or upstream-availability
  shortcut is introduced.

## Verification

```sh
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
nix flake check
```

Add black-box dry-run/confirmation, CSRF, replay, concurrency, referenced-revision,
credential-leakage, and post-deletion warm/cold request tests.

## Risks and assumptions

Issues 0030, 0045, and 0047 must complete first. Tailnet reachability alone is not
sufficient authorization for destructive archive operations.
