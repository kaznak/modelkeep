---
status: open
priority: P0
related_adrs:
  - ADR-0001
  - ADR-0002
created: 2026-08-21
updated: 2026-08-21
---

# Issue 0009: Use collision-resistant staging identifiers

- Status: Open
- Priority: P0
- Related ADR: ADR-0001, ADR-0002

## Objective

Ensure staging directory identity is unique across processes, container restarts, and concurrent operations sharing one archive.

## Problem

A process-local monotonic counter is not sufficient as the sole staging identity. Names can be reused after restart or by another process, creating the possibility that one process reuses, removes, or overwrites another process's temporary state.

## Scope

* Use a collision-resistant operation identity such as UUID/ULID/random nonce or process-instance UUID plus counter.
* Create staging paths with exclusive semantics and retry safely on collision.
* Keep operation metadata inside the staging directory rather than relying on sequential filenames.
* Coordinate the representation with Issue 0006 recovery ownership/lease handling.

## Acceptance criteria

* Concurrent processes cannot create the same staging directory.
* Restart cannot accidentally reuse existing staging.
* A forced/generated collision fails or retries safely.
* Recovery does not depend on sequential numeric staging IDs.
* Operation ownership metadata remains inspectable.

## Verification

    cargo test --all-features
    cargo clippy --all-targets --all-features -- -D warnings

Add a concurrency test creating many staging operations from multiple workers/processes against one temporary archive and assert uniqueness and intact ownership metadata.

## Risks and assumptions

Random naming alone does not solve active-versus-abandoned recovery. This issue should be implemented with, or immediately adjacent to, Issue 0006.
