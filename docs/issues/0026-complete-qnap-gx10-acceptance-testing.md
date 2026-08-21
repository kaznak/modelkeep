---
status: open
priority: P1
related_adrs:
  - ADR-0011
  - ADR-0013
created: 2026-08-21
updated: 2026-08-21
---
# Issue 0026: Complete QNAP and GX10 acceptance testing

- Status: Open
- Priority: P1
- Related ADR: ADR-0011, ADR-0013

## Objective

Complete the hardware-only MVP acceptance scenarios on the target QNAP and GX10.

## Problem

CI proves both architectures build, but it cannot prove Container Station behavior,
Tailscale Serve ingress, QNAP filesystem semantics, reboot recovery, or GX10 client
operation.

## Scope

- Deploy a pinned multi-architecture image and record QNAP/QTS/Container Station data.
- Exercise cold miss, empty-client warm hit, upstream-offline download, and Range.
- Restart the container and reboot QNAP; verify readiness, Serve, and archived data.
- Confirm LAN port 8090 is closed and tailnet HTTPS works from GX10.

## Acceptance criteria

- MVP completion conditions 1–7 and 12 have an actual target-hardware record.
- QNAP snapshot/backup and restore drill are completed for the deployed archive.
- Failures are converted into focused issues rather than waived.

## Verification

```sh
docker compose config
docker compose up -d
curl --fail http://127.0.0.1:8090/readyz
tailscale serve status
curl --fail <modelkeep-tailnet-url>/readyz
```

Follow the QNAP production runbook, run cold/warm/offline client scenarios, reboot
QNAP, repeat readiness/download checks, and attach the site acceptance record.

## Risks and assumptions

This cannot be completed in generic CI and requires access to the intended QNAP,
tailnet policy, storage share, and GX10.
