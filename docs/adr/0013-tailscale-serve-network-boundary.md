# ADR-0013: Tailscale Serve is the QNAP client network boundary

- Status: Accepted
- Date: 2026-08-21

## Context

ModelKeep speaks plain HTTP and Hugging Face clients benefit from a stable HTTPS
endpoint. The intended deployment is private to one tailnet on a QNAP NAS. Adding
Tailscale to the ModelKeep image would mix network identity and Tailscale state into
the replaceable application container and would require additional privileges.

## Decision

The supported QNAP deployment publishes the ModelKeep container port only on the
host loopback address. The official Tailscale installation on the QNAP host uses
Tailscale Serve to terminate HTTPS and proxy tailnet traffic to that loopback port.

ModelKeep does not bundle Tailscale, terminate TLS, or implement client
authentication. Tailscale Funnel is outside the supported deployment. Upstream
`HF_TOKEN` remains a server-side acquisition credential and is not client identity.
Client authentication and connection authorization occur at the tailnet layer; the
absence of a second HTTP password does not make the endpoint anonymous.

## Rationale

This preserves the non-root, capability-free, read-only application container while
providing a stable MagicDNS hostname, managed TLS, and tailnet policy enforcement.
Loopback-only publication prevents LAN clients from bypassing Tailscale Serve.

## Alternatives considered

- Bundle Tailscale in the ModelKeep image: rejected because it couples replaceable
  application state to network identity and expands container privileges and mounts.
- Expose ModelKeep directly on the LAN: rejected because it bypasses the intended
  tailnet boundary and provides no TLS.
- Manage certificates in ModelKeep: rejected because it duplicates Tailscale's
  certificate lifecycle without improving Hugging Face compatibility.

## Consequences

The QNAP host must run a Tailscale version that supports `tailscale serve`, MagicDNS,
and tailnet HTTPS certificates. Operators configure Serve separately after starting
the container. Direct host-LAN access to port 8090 no longer works by design.

Future per-user authorization may consume trusted Tailscale Serve identity or app
capability headers, but only while the backend remains unreachable except through
the trusted loopback proxy.

## Validation

`docker compose config` must show `host_ip: 127.0.0.1` for host port 8090. On QNAP,
the loopback health endpoint and the tailnet HTTPS health endpoint must both succeed,
while the QNAP LAN address on port 8090 must not accept a connection.
