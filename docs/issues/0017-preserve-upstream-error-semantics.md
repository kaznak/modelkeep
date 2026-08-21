---
status: open
priority: P1
related_adrs:
  - ADR-0005
created: 2026-08-21
updated: 2026-08-21
---
# Issue 0017: Preserve upstream error semantics through HTTP

- Status: Open
- Priority: P1
- Related ADR: ADR-0005

## Objective

Make upstream unavailable, not found, authorization, integrity, and local storage
failures distinguishable to clients and operators.

## Problem

The pull-through layer currently converts upstream failures into an archive
`InvalidPath` error and the HTTP layer converts pull-through failures broadly to
`502 Bad Gateway`. This loses operational meaning and can make an authorization or
disk failure look like a transient upstream outage.

## Scope

- Introduce a typed pull-through error boundary without exposing credentials or
  signed URLs.
- Map not found, authorization, unavailable, integrity, and storage failures to
  deliberate HTTP responses and structured events.
- Keep unsafe-path errors separate from upstream failures.
- Do not turn integrity or storage failures into cache misses.

## Acceptance criteria

- Each required failure class is distinguishable in structured logs and tests.
- Client responses are stable and evidence-based for the supported HF client.
- Tokens, helper stderr, and sensitive upstream URLs are not logged or returned.
- Existing completed revisions remain serveable during an unrelated failed fetch.

## Verification

```sh
nix develop -c cargo test --all-features
nix develop -c cargo clippy --all-targets --all-features -- -D warnings
nix flake check
```

Add black-box HTTP tests and supported-client tests for every mapped failure class.

## Risks and assumptions

Some Hugging Face client behavior depends on exact status codes. Observe the supported
client before choosing mappings rather than copying upstream responses blindly.
