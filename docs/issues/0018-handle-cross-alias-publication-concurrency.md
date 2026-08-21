---
status: open
priority: P1
related_adrs:
  - ADR-0002
  - ADR-0008
  - ADR-0010
created: 2026-08-21
updated: 2026-08-21
---
# Issue 0018: Handle cross-alias publication concurrency

- Status: Open
- Priority: P1
- Related ADR: ADR-0002, ADR-0008, ADR-0010

## Objective

Ensure concurrent acquisitions that resolve through different request keys to the
same immutable commit converge on one valid published revision.

## Problem

Single-flight is keyed by `repo@requested_revision`. Requests for `main`, a tag, and
an explicit commit can run separate acquisitions that resolve to the same commit.
One publication can win while another receives `AlreadyPublished`, causing a client
failure even though the completed revision is valid.

## Scope

- Preserve request-level single-flight without holding global locks across downloads.
- Treat a publication race as success only after verifying that the winner is a
  complete revision for the same repository and commit.
- Propagate genuine integrity or publication failures to all relevant callers.
- Keep mutable-ref updates atomic after the winning revision is verified.

## Acceptance criteria

- Concurrent `main`, tag, and commit requests resolving to one commit all succeed.
- Exactly one complete revision is observable and no staging directory is served.
- A corrupt or incomplete winner is never accepted as success.
- Failure propagation and retry behavior are deterministic.

## Verification

```sh
nix develop -c cargo test --all-features
nix develop -c cargo clippy --all-targets --all-features -- -D warnings
nix flake check
```

Add a concurrency test that controls resolution and publication ordering across at
least three aliases.

## Risks and assumptions

Deduplicating only after resolution may require a second single-flight boundary.
Correctness is more important than avoiding every duplicate upstream byte.
