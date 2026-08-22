---
status: open
priority: P1
related_adrs:
  - ADR-0006
  - ADR-0013
  - ADR-0015
created: 2026-08-22
updated: 2026-08-22
---
# Issue 0045: Add the read-only management API

- Status: Open
- Priority: P1
- Related ADR: ADR-0006, ADR-0013, ADR-0015

## Objective

Expose authenticated, read-only archive inventory and operational state for the
future management UI without requiring SSH access.

## Problem

Operators cannot inspect repositories, immutable revisions, refs, archive usage, or
recent job state from QNAP Container Station without invoking local CLI commands.
The UI needs a narrow API foundation before it can safely trigger mutations.

## Scope

- Implement the versioned read-only resources accepted by Issue 0030.
- Enumerate repositories, revisions, refs, manifest summaries, and available storage
  information from reconstructible archive state.
- Report service version, readiness, and supported management capabilities.
- Apply the management authentication and trusted-ingress policy without changing
  Hugging Face-compatible routes.
- Add bounded pagination and credential-safe errors and audit events.
- Do not add acquisition, refresh, import, deletion, or arbitrary path access.

## Acceptance criteria

- An authorized browser/API client can list and inspect archived repositories and
  revisions without SSH.
- Unauthorized, direct, and forged-header requests cannot enumerate archive content.
- Losing any derived management index does not lose model bytes or prevent rebuilding
  inventory from the archive.
- Large archives are paginated and do not require loading every manifest into memory.
- Existing `hf download`, HEAD, Range, and offline warm-hit behavior is unchanged.

## Verification

```sh
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
nix flake check
```

Add black-box authorized/unauthorized inventory tests, pagination tests, index-loss
reconstruction tests, and credential/header leakage assertions.

## Risks and assumptions

The control-plane contract is fixed by ADR-0015. Storage capacity semantics tracked
by Issue 0004 may initially expose only reliably portable values.
