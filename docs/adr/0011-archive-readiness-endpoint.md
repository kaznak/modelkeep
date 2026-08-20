# ADR-0011: Separate archive readiness from process liveness

- Status: Accepted
- Date: 2026-08-21

## Context

A running HTTP process can remain alive while the durable archive mount is missing, read-only, or unable to create temporary state. A single health endpoint cannot distinguish those conditions.

## Decision

Keep `/healthz` as a cheap process liveness endpoint. Add `/readyz` to verify the required `models` and `tmp` directories and create, sync, remove, and sync a tiny probe file in `tmp`. The probe never inspects or changes a published revision and does not perform upstream acquisition or capacity cleanup.

Container Station deployments may use `modelkeep ready` for the container healthcheck while operators retain `/healthz` for process diagnostics.

## Consequences

- Archive mount and permission failures become visible before a large fetch or import.
- Readiness probes have bounded, small filesystem overhead.
- Readiness does not guarantee free space for a model-sized operation or full archive integrity.
