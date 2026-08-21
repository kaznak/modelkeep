---
status: open
priority: P3
related_adrs:
  - ADR-0013
created: 2026-08-21
updated: 2026-08-21
---
# Issue 0021: Add tailnet identity-aware authorization

- Status: Open
- Priority: P3
- Related ADR: ADR-0013

## Objective

Optionally authorize ModelKeep operations and repositories using identity and
application capabilities supplied by the trusted Tailscale Serve ingress.

## Problem

ADR-0013 authenticates devices and restricts connectivity at the tailnet boundary,
but every client allowed to reach ModelKeep currently has the same read access. A
future shared deployment may need per-user or per-group repository policy and audit
identity without adding a second interactive login flow that breaks Hugging Face
clients.

## Scope

- Define separate data-plane (`metadata`, `HEAD`, `Range`, download) and
  administrative (`refresh`, `import`, `remove`, `audit`) capabilities.
- Evaluate Tailscale Serve identity headers and app capabilities as the authentication
  input, including behavior for tagged devices that have no user identity header.
- Accept trusted proxy headers only when direct access cannot bypass Serve; reject or
  ignore spoofed identity headers outside that boundary.
- Apply repository/action policy consistently to metadata, tree, HEAD, Range, and
  payload routes without leaking unauthorized repository existence.
- Keep tailnet client identity separate from the server-side upstream `HF_TOKEN` and
  never forward a client credential upstream.
- Produce credential-safe authorization audit events.

## Acceptance criteria

- The trust boundary and identity-to-policy mapping are recorded in an ADR before
  implementation.
- A supported real `huggingface_hub` / `hf` client can download an authorized model
  through the identity-aware ingress without protocol regressions.
- Unauthorized metadata, HEAD, Range, tree, and file requests are consistently denied
  and do not bypass ModelKeep or reveal protected payloads.
- Direct or forged identity headers cannot obtain authorization.
- Administrative capabilities are not granted to ordinary download clients.
- Upstream tokens, client credentials, signed URLs, and sensitive headers do not
  appear in logs, manifests, or responses.

## Verification

```sh
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
nix flake check
```

Add black-box proxy-boundary tests and real Hugging Face client tests for allowed and
denied metadata, HEAD, Range, and download flows. Include forged-header and credential
leakage regressions.

## Risks and assumptions

Tailscale identity headers are populated for users but not tagged devices; app
capabilities can cover both. Tailscale Serve and capability-header version requirements
must be documented. Hugging Face clients already use `Authorization`, so a second
Bearer-token meaning must not be introduced without observed compatibility evidence.
