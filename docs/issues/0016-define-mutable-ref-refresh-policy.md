---
status: open
priority: P1
related_adrs:
  - ADR-0002
  - ADR-0004
created: 2026-08-21
updated: 2026-08-21
---
# Issue 0016: Define and implement mutable-ref refresh policy

- Status: Open
- Priority: P1
- Related ADR: ADR-0002, ADR-0004

## Objective

Provide an explicit, auditable way to refresh `main` or another mutable ref while
retaining every previously archived immutable revision.

## Problem

Once a mutable ref exists locally, ordinary requests resolve it without consulting
upstream. This is safe for preservation but provides no supported way to discover and
archive a newer upstream commit. Operators cannot intentionally advance `main`
without bypassing ModelKeep's normal workflow.

## Scope

- Decide whether refresh is an administrative CLI action, an HTTP policy, or both.
- Record the durable refresh decision in a new ADR if it changes the established
  compatibility or operational contract.
- Resolve and acquire the new commit atomically before updating the mutable ref.
- Never delete or rewrite the old revision.
- Provide dry-run or equivalent preview of the current and proposed commit.

## Acceptance criteria

- Operators can explicitly refresh a mutable ref from commit A to B.
- Both A and B remain verified and downloadable after the ref moves to B.
- Failed acquisition leaves the ref at A and exposes no partial B revision.
- Normal warm-hit requests do not silently change refs unless the accepted policy
  explicitly says they should.

## Verification

```sh
nix develop -c cargo test --all-features
nix develop -c cargo clippy --all-targets --all-features -- -D warnings
nix flake check
```

Include immutable-revision, ref-update failure, and supported-client regression tests.

## Risks and assumptions

Automatic time-based refresh would introduce upstream dependency and policy ambiguity.
Prefer an explicit administrative boundary unless protocol evidence requires another
behavior.
