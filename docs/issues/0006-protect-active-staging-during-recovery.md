---
status: open
priority: P0
related_adrs:
  - ADR-0001
  - ADR-0002
created: 2026-08-21
updated: 2026-08-21
---

# Issue 0006: Protect active staging during recovery

- Status: Open
- Priority: P0
- Related ADR: ADR-0001, ADR-0002

## Objective

Prevent recovery or management commands from deleting staging data that belongs to an active fetch, import, or publication.

## Problem

The reviewed recovery path can clean the temporary staging area broadly. If a server is downloading a large model while another ModelKeep process runs a management command against the same archive, active staging may be mistaken for abandoned work. That can abort acquisition, force large re-downloads, or create races around publication.

## Scope

* Separate active work from abandoned staging with collision-resistant operation identity plus ownership/lease/locking metadata.
* Do not run mutating recovery as a side effect of read-only commands such as `list`, `show`, or `verify`.
* Keep recovery conservative and idempotent.
* Do not rely on PID alone as proof of ownership.

## Acceptance criteria

* Read-only commands cannot delete or invalidate an active staging tree.
* Concurrent processes cannot accidentally claim the same staging operation.
* Deliberately abandoned staging is recoverable.
* Crash recovery remains idempotent.
* Tests cover active and abandoned staging against the same archive root.

## Verification

    cargo test --all-features
    cargo clippy --all-targets --all-features -- -D warnings

Add an integration scenario that keeps a staging operation active, invokes management commands, verifies the staging tree survives, then simulates owner loss and verifies recovery can remove the abandoned staging.

## Risks and assumptions

Lock and lease behavior can vary across filesystems used by QNAP/container bind mounts. The design must fail conservatively when ownership cannot be established.
