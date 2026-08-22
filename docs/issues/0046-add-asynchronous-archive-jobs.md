---
status: open
priority: P1
related_adrs:
  - ADR-0002
  - ADR-0008
  - ADR-0009
  - ADR-0012
created: 2026-08-22
updated: 2026-08-22
---
# Issue 0046: Add asynchronous archive jobs

- Status: Open
- Priority: P1
- Related ADR: ADR-0002, ADR-0008, ADR-0009, ADR-0012

## Objective

Allow authorized operators to start and monitor long-running archive operations
through the management API instead of running local commands.

## Problem

Large model acquisition, refresh, verification, and audit can run longer than an HTTP
request or browser session. Treating them as synchronous endpoints would make timeout,
retry, duplicate execution, restart, and progress behavior ambiguous.

## Scope

- Add durable/reconstructible job resources for prefetch, mutable-ref refresh,
  revision verification, and archive audit.
- Define queued, resolving, downloading, verifying, publishing, completed, failed,
  and cancelled states where cancellation is safe.
- Provide progress based only on trustworthy upstream/file information; represent
  unknown totals explicitly.
- Apply idempotency keys and existing single-flight behavior to duplicate acquisition.
- Recover or clearly fail interrupted jobs without publishing partial revisions.
- Keep server-side import paths and revision deletion outside this issue.

## Acceptance criteria

- A first-time prefetch of `<repo>@<ref>` publishes a complete immutable snapshot and
  returns a monitorable job without a Hugging Face client request.
- Refresh preserves the old revision and updates the ref only after complete publish.
- Retrying an idempotent request does not start a duplicate large download.
- Restart and cancellation never expose partial archive data as complete.
- Job errors distinguish authorization, upstream, integrity, storage, cancellation,
  and internal metadata failures without leaking credentials or signed URLs.

## Verification

```sh
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
nix flake check
```

Run black-box cold prefetch, duplicate submission, restart, cancellation, disk-full,
credential-leakage, and upstream-offline warm-download scenarios.

## Risks and assumptions

Issues 0030 and 0045 must define and implement the control-plane foundation first.
Job metadata is operational state and must not become the authority for model bytes.
