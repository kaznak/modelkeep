---
status: open
priority: P1
related_adrs:
  - ADR-0008
created: 2026-08-22
updated: 2026-08-22
---
# Issue 0043: Add startup and lifecycle logging

- Status: Open
- Priority: P1
- Related ADR: ADR-0008

## Objective

Make ModelKeep startup, archive initialization, readiness, and shutdown observable
from QNAP Container Station logs without requiring SSH access.

## Problem

The deployed containers can remain silent during normal startup and ownership
initialization. An operator therefore cannot tell from Container Station whether the
archive was prepared, ModelKeep started listening, readiness was reached, or startup
failed before serving traffic.

Issue 0027 covers detailed request, cache, fetch, verification, and recovery events.
This issue is limited to the basic process and deployment lifecycle needed to
diagnose startup from the QNAP GUI.

## Scope

- Emit a startup event containing the ModelKeep version, configured listen address,
  archive root, and whether pull-through acquisition is enabled.
- Emit archive initialization/recovery start and completion events, including
  credential-safe failure classification when initialization cannot complete.
- Make the QNAP ownership initialization container report start, target, result, and
  completion or failure to its standard output/error.
- Emit an explicit ready event only after ModelKeep can serve requests.
- Emit graceful-shutdown start and completion events when the process receives a
  supported termination signal.
- Keep routine health checks from flooding the default log while retaining enough
  information to diagnose health-check failures.
- Preserve `RUST_LOG` as the operator-facing log filter and document practical
  `info` and `debug` settings for Container Station.
- Emit successful `/healthz` and `/readyz` probe events at `debug`, while keeping
  failed readiness checks at an operator-visible warning level.
- Document the expected startup event sequence in the QNAP deployment runbook.

## Acceptance criteria

- A fresh QNAP Compose deployment shows ownership initialization, ModelKeep startup,
  and readiness in chronological order in Container Station logs.
- A restarted deployment shows the same lifecycle and identifies any incomplete
  archive recovery without exposing partial data as complete.
- Permission failure, invalid configuration, archive initialization failure, and
  listen-address conflict produce distinguishable operator-facing errors and a
  non-zero container exit status where startup cannot continue.
- Startup and failure logs do not contain `HF_TOKEN`, authorization headers, signed
  URLs, or other credentials.
- Default logs do not emit one request event for every successful `/healthz` probe.
- With `RUST_LOG=debug`, successful `/healthz` and `/readyz` requests are visible and
  distinguishable; with the default `RUST_LOG=info`, they remain quiet.
- Changing `RUST_LOG` to a valid supported filter changes verbosity without rebuilding
  the image, and an invalid filter fails clearly or falls back according to a
  documented policy.
- The event names and stable fields are covered by log-capture tests.
- The QNAP deployment documentation shows how to find both initialization and
  ModelKeep service logs in Container Station.

## Verification

```sh
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
docker compose config
nix flake check
```

Additionally exercise successful startup, ownership failure, invalid archive
configuration, occupied listen address, `SIGTERM`, and repeated health probes while
capturing standard output/error.

## Risks and assumptions

- Archive paths and repository identifiers may be logged, but credentials and
  upstream signed URLs must not be logged.
- Ownership initialization currently runs outside the ModelKeep process; its log
  contract belongs to the deployment configuration while sharing the same
  credential-safety requirements.
- This issue does not add metrics, tracing infrastructure, or the request/fetch event
  coverage tracked by Issues 0004 and 0027.
