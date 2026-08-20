---
status: in_progress
priority: P1
related_adrs: []
created: 2026-08-21
updated: 2026-08-21
---
# Issue 0003: Add a health endpoint

- Status: Open
- Priority: P1
- Related ADR: None

## Objective

Provide a lightweight liveness endpoint for QNAP Container Station and deployment
monitoring.

## Problem

The container currently has no dedicated endpoint that distinguishes a running HTTP
server from a model file request. Container health configuration therefore requires
an external probe or a model-specific URL.

## Scope

Add an endpoint such as:

```text
GET /healthz
```

It should be cheap, unauthenticated within the deployment network, and independent of
upstream Hugging Face availability. Define separately whether archive storage
availability is part of readiness.

## Acceptance criteria

- A healthy server returns a stable 2xx response.
- The endpoint does not trigger an upstream fetch.
- Compose can use it as a healthcheck without adding a large runtime dependency.
- Tests cover the endpoint and an unavailable archive root if readiness checks include
  storage.

## Verification

```sh
cargo test --all-features
docker compose config
```

## Risks and assumptions

Liveness and readiness should not be conflated. A temporary upstream outage should not
make a server that can still serve archived revisions appear dead.

## Deployment research: QNAP Container Station

Container Station 3 manages Docker Compose applications, so the standard Compose healthcheck field is the appropriate integration point.

Important operational distinctions:

- healthcheck classifies the container as healthy or unhealthy.
- restart: unless-stopped restarts a terminated container, but unhealthy alone does not normally restart it.
- Compose can wait for service_healthy dependencies.
- Container Station UI behavior may vary by QTS and Container Station version.

The current image does not guarantee curl or wget for an in-container probe. Add a small probe command or equivalent ModelKeep command before adding the Compose healthcheck stanza.

References:

- https://www.qnap.com/en-us/how-to/tutorial/article/how-to-use-container-station-3
- https://docs.docker.com/reference/compose-file/services/
