---
status: open
priority: P3
related_adrs:
  - ADR-0001
  - ADR-0006
created: 2026-08-21
updated: 2026-08-21
---
# Issue 0033: Add an S3 storage backend

- Status: Open
- Priority: P3
- Related ADR: ADR-0001, ADR-0006

## Objective

Evaluate S3-compatible storage without sacrificing the ordinary-file QNAP archive as
an independently recoverable durable representation.

## Problem

Object storage has different atomicity and Range semantics and cannot silently replace
the accepted filesystem durability contract.

## Scope

Write an ADR defining whether S3 is replica, import/export target, or new authority;
then implement only the selected role.

## Acceptance criteria

Partial uploads are never complete, Range is correct, and existing archives require
no redownload or opaque migration.

## Verification

```sh
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
nix flake check
```

Run failure-injection, integrity, Range, and recovery tests against a pinned emulator.

## Risks and assumptions

This is an architectural change and must not be introduced as a generic abstraction.
