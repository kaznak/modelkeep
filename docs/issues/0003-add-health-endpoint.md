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
