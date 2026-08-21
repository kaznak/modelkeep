---
status: open
priority: P3
related_adrs:
  - ADR-0001
  - ADR-0006
created: 2026-08-21
updated: 2026-08-21
---
# Issue 0034: Add an OCI model registry backend

- Status: Open
- Priority: P3
- Related ADR: ADR-0001, ADR-0006

## Objective

Evaluate OCI/model registries as an explicit distribution or replica target.

## Problem

Registry manifests and layers are not ordinary materialized model files and must not
become the sole durable representation accidentally.

## Scope

Define artifact mapping, immutable identity, authentication, large-layer behavior,
and import/export boundaries in an ADR before implementation.

## Acceptance criteria

Registry loss never makes the QNAP archive unreadable, and exported revisions verify
byte-for-byte after import.

## Verification

```sh
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
nix flake check
```

Run large artifact, interrupted push/pull, authentication, and round-trip scenarios.

## Risks and assumptions

Registry size limits and garbage collection differ by implementation.
