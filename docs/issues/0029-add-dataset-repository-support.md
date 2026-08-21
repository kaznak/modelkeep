---
status: open
priority: P3
related_adrs:
  - ADR-0001
  - ADR-0005
created: 2026-08-21
updated: 2026-08-21
---
# Issue 0029: Add dataset repository support

- Status: Open
- Priority: P3
- Related ADR: ADR-0001, ADR-0005

## Objective

Extend the evidence-driven compatibility surface to Hugging Face dataset repositories.

## Problem

Paths, API metadata, and fetch requests currently assume `repo_type=model`.

## Scope

Observe supported dataset clients, define collision-free durable paths, extend the
official fetcher boundary, and add cold/warm/offline tests.

## Acceptance criteria

Dataset support cannot collide with model archives and remains offline-downloadable.

## Verification

```sh
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
nix flake check
```

Run dataset cold-miss and empty-cache offline downloads with a supported real client.

## Risks and assumptions

This requires an ADR if the durable archive namespace changes.
