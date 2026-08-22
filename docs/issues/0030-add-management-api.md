---
status: open
priority: P1
related_adrs:
  - ADR-0006
  - ADR-0007
  - ADR-0012
  - ADR-0013
created: 2026-08-21
updated: 2026-08-22
---
# Issue 0030: Define the management control-plane contract

- Status: Open
- Priority: P1
- Related ADR: ADR-0006, ADR-0007, ADR-0012, ADR-0013

## Objective

Define the trust boundary, resource model, and compatibility contract for a
GUI-oriented management control plane before implementing its API.

## Problem

Refresh, acquisition, verification, removal, and audit are local CLI operations.
QNAP operators should not need SSH or `docker exec`, but exposing these operations
beside the Hugging Face-compatible download surface would introduce authentication,
CSRF, idempotency, and destructive-action risks.

## Scope

- Record an ADR separating the download data plane from the management control plane.
- Decide the tailnet hostname, Tailscale ACL, backend authentication, trusted-proxy,
  browser-session, and CSRF boundaries.
- Define stable repository, revision, ref, capacity, job, and error resources without
  making a database the authority for archived bytes.
- Define idempotency, concurrency, audit-event, pagination, and API-version policy.
- Separate read-only inventory, non-destructive jobs, and destructive deletion
  capabilities.
- Exclude arbitrary server filesystem paths and bearer-token meanings that conflict
  with Hugging Face clients.
- Specify the implementation sequence tracked by Issues 0045–0048.

## Acceptance criteria

- The accepted ADR makes the management surface unreachable through ordinary
  Hugging Face download routes and defines how direct/forged proxy headers fail.
- Browser authentication and CSRF behavior are explicit for a tailnet-only service.
- The API model can be reconstructed from durable archive state where practical.
- Acquisition and verification are asynchronous resources; request timeouts do not
  define job success or failure.
- Deletion remains an explicit, separately authorized workflow preserving ADR-0007.
- Issues 0045–0048 can be implemented without inventing incompatible contracts.

## Verification

```sh
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
nix flake check
```

Review the ADR against black-box authorization, idempotency, crash-safety, CSRF, and
credential-leakage scenarios before API implementation.

## Risks and assumptions

This issue makes an architectural decision only. Do not expose local filesystem paths,
weaken explicit deletion/refresh semantics, or grant administration merely because a
request originates somewhere in the tailnet.
