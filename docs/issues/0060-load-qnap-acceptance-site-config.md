---
status: in_progress
priority: P1
related_adrs:
  - ADR-0013
  - ADR-0015
created: 2026-09-22
updated: 2026-09-22
---
# Issue 0060: Load QNAP acceptance site configuration from a local file

- Status: In Progress
- Priority: P1
- Related ADR: ADR-0013, ADR-0015

## Objective

Initialize QNAP acceptance records from one Git-ignored site configuration file so
private endpoints and deployment details never need to appear in commands or tracked
documentation.

## Problem

The documented `init` command requires many literal flags. Copying the example can
silently put the administration endpoint in both endpoint fields and retain sample
repository, digest, and site metadata. This causes an opaque `/healthz` 404 during
preflight and risks copying private site values into shell history or documentation.

## Acceptance criteria

- `init` loads all site-specific values from a JSON configuration file.
- The default config and generated record paths are ignored by Git.
- Tracked examples contain only reserved example domains and addresses.
- Download and administration endpoints must be distinct HTTPS origins.
- Documentation provides short copyable commands after the one-time config is filled.
- Tests cover config loading, distinct endpoint validation, and record creation.

## Verification

```sh
python3 -m unittest -v tests/test_qnap_client_acceptance.py
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
nix flake check
```

## Risks and assumptions

The local config and generated record contain operational site information but must
not contain credentials. The acceptance record intentionally retains the evaluated
configuration as evidence and must remain outside Git.
