---
status: open
priority: P3
related_adrs:
  - ADR-0005
  - ADR-0013
created: 2026-08-21
updated: 2026-08-21
---
# Issue 0028: Define private and gated repository credential policy

- Status: Open
- Priority: P3
- Related ADR: ADR-0005, ADR-0013

## Objective

Define and test supported upstream credentials for successful private/gated archive
acquisition without confusing them with tailnet client identity.

## Problem

Server-side `HF_TOKEN` is supported and error classes are tested, but credential
ownership, per-repository policy, rotation, and a successful gated flow are not yet a
recorded compatibility contract.

## Scope

- Decide service-token versus delegated-client-token policy in an ADR.
- Test successful acquisition and offline serving without credential persistence.
- Specify rotation and revocation behavior.

## Acceptance criteria

- Credentials never enter URLs, logs, manifests, or archive metadata.
- Archived authorized data remains serveable offline according to explicit policy.

## Verification

```sh
cargo test --all-features
nix flake check
```

Run a controlled successful private/gated acquisition, remove upstream access, and
repeat download while asserting credential absence from logs and durable files.

## Risks and assumptions

Forwarding client Bearer tokens can conflict with future ModelKeep authorization and
must not be implemented without observed client behavior.
