---
status: open
priority: P2
related_adrs:
  - ADR-0013
  - ADR-0014
created: 2026-08-22
updated: 2026-08-22
---
# Issue 0044: Improve QNAP container identification

- Status: Open
- Priority: P2
- Related ADR: ADR-0013, ADR-0014

## Objective

Make the ModelKeep application and its completed ownership initializer easy to
identify and interpret in QNAP Container Station.

## Problem

Container Station applies Compose's generated
`<project>-<service>-<replica>` names, producing names such as
`modelkeep-modelkeep-1`. The short-lived ownership initializer also appears stopped
or under an application-level "other" status after successful completion. Both are
valid Compose behavior, but they make normal operation look ambiguous in the QNAP
GUI.

Compose has no Kubernetes Pod-level init-container status to merge into the
long-running service. The deployment must improve identification without keeping a
privileged initializer running or weakening the non-root application boundary.

## Scope

- Assign concise, deterministic container names to the single-instance QNAP
  deployment (`modelkeep` and `modelkeep-init`).
- Document that the fixed names intentionally make multiple copies of this Compose
  application on one Docker host conflict unless operators rename them.
- Document Container Station's expected steady state: ModelKeep running and healthy,
  initializer stopped with exit code zero.
- Add GUI-oriented troubleshooting guidance for distinguishing a successfully
  completed initializer from an initialization failure.
- Keep lifecycle output itself in Issue 0043 and preserve the ownership/security
  design accepted by ADR-0014.

## Acceptance criteria

- A QNAP Compose deployment creates containers named `modelkeep` and
  `modelkeep-init`, without the generated project and replica suffixes.
- Compose normalization confirms both fixed names and retains
  `service_completed_successfully` dependency gating.
- The initializer still exits after its one-time operation; it is not converted into
  an idle long-running root container.
- The ModelKeep service remains UID/GID `10001:10001`, capability-free, and bound only
  to host loopback.
- The QNAP runbook explains that exit code zero for the stopped initializer is the
  expected successful state and identifies the failure indicators to inspect.
- The runbook notes the fixed-name collision constraint for multiple deployments on
  one QNAP host.

## Verification

```sh
docker compose config
docker compose up -d
docker inspect modelkeep-init --format '{{.State.Status}} {{.State.ExitCode}}'
docker inspect modelkeep --format '{{.Name}} {{.State.Status}}'
```

Also verify from Container Station that the two concise names are displayed, the
initializer's zero exit status is discoverable, and ModelKeep becomes healthy.

## Risks and assumptions

- Container Station may continue to summarize the overall Compose application as
  "other" because one service is intentionally completed; this issue improves the
  explanation and per-container signals rather than promising control over QNAP's
  application-status label.
- Fixed `container_name` values trade Compose scaling and parallel deployments for
  simpler single-instance QNAP operation. ModelKeep's supported QNAP topology is one
  instance per host.
