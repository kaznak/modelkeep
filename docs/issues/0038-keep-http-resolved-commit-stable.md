---
status: open
priority: P1
related_adrs:
  - ADR-0002
  - ADR-0012
created: 2026-08-21
updated: 2026-08-21
---
# Issue 0038: Keep resolved commits stable across HTTP responses

- Status: Open
- Priority: P1
- Related ADR: ADR-0002, ADR-0012

## Objective

Use one immutable commit identity for file selection, ETag, and `x-repo-commit` in a
single HTTP response.

## Problem

Mutable refs are resolved once to select a file and again to create response headers.
A concurrent refresh can therefore label old bytes with the new commit identity.

## Acceptance criteria

- Each request carries the commit used to resolve its file through response creation.
- A concurrent ref update cannot change that response's ETag or commit header.
- Cold misses use the commit returned by pull-through publication.

## Verification

```sh
cargo test --all-features
nix flake check
```

Add a deterministic HTTP regression test covering a ref move during one response.

## Risks and assumptions

This changes no ETag format; it only makes the existing identity internally
consistent.
