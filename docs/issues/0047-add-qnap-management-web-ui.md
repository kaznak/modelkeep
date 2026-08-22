---
status: open
priority: P1
related_adrs:
  - ADR-0013
created: 2026-08-22
updated: 2026-08-22
---
# Issue 0047: Add the QNAP management web UI

- Status: Open
- Priority: P1
- Related ADR: ADR-0013

## Objective

Provide a small tailnet-only management UI for routine QNAP operation without SSH or
local CLI commands.

## Problem

Even with an API, operators need a practical way to browse the archive, start model
prefetch/refresh, and understand long-running progress and failures from a browser.

## Scope

- Build a replaceable UI on the versioned APIs from Issues 0045 and 0046.
- Show service/readiness state, repositories, revisions, refs, capacity, jobs, and
  credential-safe failure details.
- Allow prefetch, refresh, verify, and audit submission and job monitoring.
- Document a distinct Tailscale Service hostname and ACL for the admin surface.
- Support keyboard access, narrow/mobile layouts, and multi-gigabyte job progress
  without polling unbounded payloads.
- Do not expose upstream tokens, arbitrary filesystem paths, upload/import, or
  deletion controls.

## Acceptance criteria

- A QNAP operator can prefetch a model and confirm publication entirely through the
  browser over the approved tailnet admin hostname.
- Refresh/reload or browser disconnect does not lose the server-side job.
- Unauthorized users and ordinary download clients cannot load management data or
  trigger jobs.
- The UI clearly distinguishes queued/running/failed/completed operations and unknown
  progress totals.
- ModelKeep remains fully operable through its API and CLI if the UI is removed.

## Verification

```sh
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
nix flake check
```

Add browser-level authentication, CSRF, accessibility, job-resume, error rendering,
and QNAP/Tailscale deployment tests.

## Risks and assumptions

Issues 0030, 0045, and 0046 must complete first. The UI is not durable state and must
not introduce a second archive metadata authority.
