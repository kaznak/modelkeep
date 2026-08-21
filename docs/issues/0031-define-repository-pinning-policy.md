---
status: open
priority: P3
related_adrs:
  - ADR-0002
  - ADR-0004
  - ADR-0007
created: 2026-08-21
updated: 2026-08-21
---
# Issue 0031: Define repository pinning policy

- Status: Open
- Priority: P3
- Related ADR: ADR-0002, ADR-0004, ADR-0007

## Objective

Define whether pin/unpin metadata adds useful operator intent without becoming
automatic garbage collection.

## Problem

All revisions are retained until explicit removal; future operators may want a marker
that protects selected repositories or revisions from reviewed removal workflows.

## Scope

Record semantics, durable representation, reconstruction, and interaction with refs
and explicit deletion before implementation.

## Acceptance criteria

Pinning never deletes data automatically and cannot make mutable refs destructive.

## Verification

```sh
cargo test --all-features
nix flake check
```

Run immutable-ref, deletion, reconstruction, and crash-focused tests.

## Risks and assumptions

An unused pin concept would add metadata complexity; require a concrete workflow first.
