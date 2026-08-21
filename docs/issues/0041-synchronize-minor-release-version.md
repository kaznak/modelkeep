---
status: open
priority: P1
related_adrs: []
created: 2026-08-21
updated: 2026-08-21
---
# Issue 0041: Synchronize the minor-release version

- Status: Open
- Priority: P1
- Related ADR: None

## Objective

Prepare the next minor release as version `0.2.0` across Rust, Nix, OCI, Compose,
workflow, and release documentation.

## Problem

All current artifact and deployment identifiers still say `0.1.0`. Tagging `v0.2.0`
would publish an externally renamed image whose package metadata and defaults remain
at the previous version.

## Acceptance criteria

- Cargo package, Nix package/check versions, internal OCI tag, Compose default,
  workflow source tag, and release examples agree on `0.2.0` / `v0.2.0`.
- The release image builds and the multi-architecture workflow still addresses the
  image it loaded.

## Verification

```sh
rg '0\.1\.0|v0\.1\.0' Cargo.toml flake.nix compose.yaml .github README.md README.ja.md
nix flake check
nix build .#packages.x86_64-linux.modelkeep-image
```

The search must return no stale release identifiers.

## Risks and assumptions

The requested minor release is `0.2.0`; no compatibility promise beyond semantic
versioning is introduced.
