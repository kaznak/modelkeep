---
status: open
priority: P2
related_adrs:
  - ADR-0003
  - ADR-0005
created: 2026-08-21
updated: 2026-08-21
---
# Issue 0005: Define the client network boundary

- Status: Open
- Priority: P2
- Related ADR: ADR-0003, ADR-0005

## Objective

Document and, where necessary, enforce how clients are allowed to access ModelKeep
without exposing the archive service beyond its intended network boundary.

## Problem

The current server does not provide client authentication or TLS. This may be
acceptable on a trusted private LAN, but the deployment contract is not yet explicit.

## Scope

- Document the supported trusted-network deployment model.
- Decide whether TLS and client authentication belong in ModelKeep or a reverse proxy.
- Keep upstream `HF_TOKEN` handling separate from client authorization.
- Do not put credentials in URLs, manifests, logs, or archive metadata.

## Acceptance criteria

- The QNAP deployment documents its intended network exposure.
- A reverse-proxy deployment path is documented if ModelKeep remains HTTP-only.
- Upstream credentials cannot be returned to clients or written to durable archive
  state.
- Any new authentication behavior has black-box protocol tests.

## Verification

Review the Compose deployment and run the security tests for path traversal and
credential leakage before changing the trust boundary.

## Risks and assumptions

Adding authentication directly to the compatibility layer can affect standard
Hugging Face client behavior. A reverse proxy may be the smaller operational boundary.
