---
status: open
priority: P1
related_adrs:
  - ADR-0001
  - ADR-0004
  - ADR-0006
created: 2026-08-21
updated: 2026-08-21
---
# Issue 0019: Add the QNAP production, backup, and recovery runbook

- Status: Open
- Priority: P1
- Related ADR: ADR-0001, ADR-0004, ADR-0006

## Objective

Provide a tested operator procedure for first deployment, backup, restore, upgrade,
rollback, and disaster recovery on QNAP before durable model data is entrusted to the
service.

## Problem

The repository documents permissions and basic Compose startup, but not a complete
production lifecycle. The archive is durable state, so an untested snapshot, restore,
or image upgrade procedure can create false confidence even when ModelKeep itself is
correct.

## Scope

- Define supported QNAP share, snapshot, and external-backup responsibilities.
- Document preflight, pinned image selection, startup, health/readiness inspection,
  log inspection, upgrade, and rollback.
- Document a consistent backup boundary for `models`, refs, manifests, and staging.
- Perform a restore drill into a separate path and verify restored revisions without
  upstream access.
- Include an incident procedure for disk full, mount loss, interrupted acquisition,
  and a failed container upgrade.

## Acceptance criteria

- A new operator can deploy a pinned image without using `latest`.
- At least one archived revision is restored to a clean path, verified, and served
  with upstream blocked.
- Rollback replaces the container image without modifying archived revisions.
- Backup credentials and upstream tokens are not stored in the archive or runbook.

## Verification

```sh
docker compose config
docker compose run --rm modelkeep ready
modelkeep verify /restored-data <repo-id> <commit>
```

Record the tested QNAP/Container Station versions, filesystem, snapshot mechanism,
image digest, and restore-drill result in the deployment documentation.

## Risks and assumptions

QNAP snapshot and ACL behavior varies by model, firmware, and filesystem. The runbook
must name the tested environment and clearly identify storage-layer responsibilities.
