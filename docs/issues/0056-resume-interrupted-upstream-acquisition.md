---
status: open
priority: P1
related_adrs:
  - ADR-0008
  - ADR-0009
  - ADR-0010
created: 2026-08-24
updated: 2026-08-24
---
# Issue 0056: Resume interrupted upstream acquisition safely

- Status: Open
- Priority: P1
- Related ADR: ADR-0008, ADR-0009, ADR-0010

## Objective

Allow a new request or management job to resume reusable upstream download staging
after a container restart or recreation, without exposing or publishing incomplete
model data.

## Problem

An interrupted acquisition remains under `/data/tmp` until its staging lease expires,
but a retry allocates a new staging directory and downloads from the beginning. For a
large sharded model this can waste hours, temporarily double storage consumption, and
increase the chance of ENOSPC. The current behavior is data-safe because partial data
is never published, but it is operationally inefficient.

Management jobs that were active at restart should remain explicit interrupted
failures for auditability. Resumption belongs to a new request or job and must not
silently rewrite the history of the interrupted operation.

## Scope

- Persist enough credential-free acquisition identity to determine whether abandoned
  staging matches the repository, requested revision, resolved commit, and fetch
  selection of a new acquisition.
- Define an exclusive, crash-safe lease handoff from expired staging to one new
  operation; active or ambiguous staging must never be adopted.
- Delegate byte-level continuation to the supported official Hugging Face client only
  where its `local_dir` behavior is demonstrated to resume safely.
- Validate all resumed output through the existing complete-snapshot publication
  boundary before removing the lease and atomically publishing the revision.
- Remove or conservatively quarantine incompatible, corrupt, or unidentifiable
  staging without treating it as a cache hit or completed revision.
- Emit operational events that distinguish resumed, discarded, and newly started
  acquisition without logging credentials or signed URLs.

## Acceptance criteria

- Recreating the container during a fixture-backed multi-file acquisition leaves no
  partial revision observable through ModelKeep.
- A later retry adopts exactly one matching expired staging directory and transfers
  fewer upstream bytes than a complete restart of the same acquisition.
- Concurrent retries cannot adopt the same staging directory or perform duplicate
  publication.
- Active leases, mismatched repository/revision/commit/file selection, malformed
  metadata, and failed integrity checks are never resumed as trusted data.
- The resumed acquisition publishes only after the normal size, digest, manifest,
  flush, and atomic-rename checks succeed.
- The original management job remains `failed` with phase `interrupted`; the new job
  records whether it resumed prior staging.
- Restarting with upstream unavailable leaves the incomplete staging unpublished and
  returns an operationally meaningful failure rather than a false warm hit.

## Verification

```sh
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
nix flake check
```

Add black-box crash/recreation tests covering resumable success, incompatible
staging, concurrent adoption, integrity failure, upstream-offline retry, and ENOSPC
behavior. Run the native amd64 and arm64 GitHub Actions checks. Before closing this
issue, record one QNAP container recreation during a representative large prefetch
and confirm that transferred bytes and temporary capacity behave as designed.

## Risks and assumptions

The supported Hugging Face client may change its partial-download metadata or resume
behavior. ModelKeep must treat that state as an optimization, not durable archive
format or authority. If safe compatibility cannot be demonstrated for a client
version, starting a new acquisition is preferable to adopting ambiguous bytes.
