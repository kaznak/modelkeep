---
status: open
priority: P2
related_adrs:
  - ADR-0002
created: 2026-08-21
updated: 2026-08-21
---
# Issue 0039: Recognize only full Hugging Face commit IDs

- Status: Open
- Priority: P2
- Related ADR: ADR-0002

## Objective

Treat only 40-character hexadecimal Hugging Face commit IDs as immutable revisions.

## Problem

The HTTP and pull-through layers currently classify every non-empty hexadecimal
string as a commit, so valid mutable names such as `deadbeef` bypass ref resolution
and ref updates.

## Acceptance criteria

- Forty hexadecimal characters are classified as commits.
- Short hexadecimal branch and tag names are classified as mutable refs.
- HTTP and pull-through share one definition.

## Verification

```sh
cargo test --all-features
nix flake check
```

Add unit and pull-through regressions for a `deadbeef` ref.

## Risks and assumptions

The supported Hugging Face model protocol uses full 40-character commit identities.
