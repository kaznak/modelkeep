---
status: open
priority: P3
related_adrs:
  - ADR-0001
  - ADR-0006
created: 2026-08-21
updated: 2026-08-21
---
# Issue 0036: Add non-Hugging-Face upstream sources

- Status: Open
- Priority: P3
- Related ADR: ADR-0001, ADR-0006

## Objective

Add one evidence-backed non-Hugging-Face upstream without weakening archive invariants.

## Problem

The name ModelKeep is source-neutral, but all current resolution, acquisition, and
client compatibility behavior is Hugging Face-specific.

## Scope

Select a concrete source and client workflow, define immutable identity and namespace
in an ADR, and keep the fetch adapter replaceable.

## Acceptance criteria

The new source cannot collide with HF archives and remains offline-serveable from
ordinary durable files.

## Verification

```sh
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
nix flake check
```

Run source-specific cold/warm/offline, integrity, crash, and namespace scenarios.

## Risks and assumptions

Do not generalize around hypothetical sources; require one concrete supported system.
