---
status: open
priority: P3
related_adrs:
  - ADR-0001
  - ADR-0006
created: 2026-08-21
updated: 2026-08-21
---
# Issue 0035: Add archive export and import interoperability

- Status: Open
- Priority: P3
- Related ADR: ADR-0001, ADR-0006

## Objective

Define verified export/import interoperability with ModelArk or another archival tool.

## Problem

Ordinary files are inspectable, but no portable exchange manifest or supported
round-trip workflow exists.

## Scope

Choose a concrete counterpart and use case, map immutable revisions and checksums, and
preserve reconstructibility without adopting its internal cache as authority.

## Acceptance criteria

Round-tripped model bytes and commit identity verify without upstream access.

## Verification

```sh
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
nix flake check
```

Run deterministic export/import, corruption, missing-file, and offline-serving tests.

## Risks and assumptions

Do not design a speculative universal archive format without a supported counterpart.
