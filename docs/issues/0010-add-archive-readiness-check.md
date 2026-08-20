---
status: open
priority: P1
related_adrs:
  - ADR-0004
  - ADR-0006
created: 2026-08-21
updated: 2026-08-21
---

# Issue 0010: Add archive readiness check

- Status: Open
- Priority: P1
- Related ADR: ADR-0004, ADR-0006

## Objective

Expose a readiness signal that distinguishes a live ModelKeep process from an archive that is actually usable.

## Problem

A trivial liveness endpoint can remain healthy while the QNAP archive is missing, read-only, inaccessible due to ACL changes, or unable to create staging. Operators and Container Station therefore need a separate readiness signal.

## Scope

* Keep `/healthz` as process liveness.
* Add `/readyz` for archive readiness.
* Check required archive paths and, where write capability is required, safely create and remove a tiny object in the designated temporary area.
* Never modify a published revision from a readiness probe.
* Keep capacity policy separate; readiness must not trigger deletion.

## Acceptance criteria

* `/healthz` can remain successful when only the archive is unhealthy.
* `/readyz` fails when required archive state is unavailable or unwritable.
* Probes leave no persistent garbage.
* Repeated probes do not alter published revisions.
* QNAP/Compose documentation uses the appropriate endpoint.

## Verification

    cargo test --all-features
    cargo clippy --all-targets --all-features -- -D warnings

Test a normal archive, a missing archive root, an unwritable temporary area where practical, cleanup after probing, and unchanged published revisions.

## Risks and assumptions

Filesystem permission and mount behavior differs across QNAP/container environments. Readiness should report what failed without becoming an expensive full integrity check.
