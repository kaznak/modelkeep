---
status: done
priority: P1
related_adrs: []
created: 2026-08-21
updated: 2026-08-21
---
# Issue 0012: Make Clippy validation reproducible

- Status: Done
- Priority: P1
- Related ADR: None

## Objective

Provide Cargo, Rustc, Rustfmt, and Clippy from one pinned Nix toolchain so the
documented validation commands do not depend on tools installed in the user's
profile.

## Problem

The current flake has no explicit development shell. `nix develop` derives an
environment from the default package, which provides Cargo and Rustc but not
Clippy. Running `cargo clippy` can therefore find an unrelated `cargo-clippy`
from the user's `PATH`. If its compiler version differs from the flake's Rustc,
validation fails with incompatible crate metadata even after `cargo clean`.

## Write scope

- `flake.nix`
- this issue record
- development documentation only if the supported command changes

## Do not touch

- archive representation, publication, and recovery
- HTTP compatibility and upstream acquisition
- accepted ADRs
- unrelated dependencies or formatting

## Acceptance criteria

- The flake exposes an explicit development shell for every supported system.
- Cargo, Rustc, Rustfmt, and Clippy in that shell come from the pinned nixpkgs
  input and use a compatible Rust toolchain version.
- `cargo clippy --all-targets --all-features -- -D warnings` succeeds inside
  `nix develop` without relying on a user-profile Clippy.
- Existing tests and flake checks continue to pass.

## Verification

```sh
nix develop -c sh -c 'command -v cargo-clippy; rustc --version; cargo clippy --version'
nix develop -c cargo fmt --check
nix develop -c cargo clippy --all-targets --all-features -- -D warnings
nix develop -c cargo test --all-features
nix flake check
```

## Risks and assumptions

The pinned nixpkgs revision is assumed to provide compatible `cargo`, `rustc`,
`rustfmt`, and `clippy` packages on both `x86_64-linux` and `aarch64-linux`.
Cross-system evaluation does not replace an aarch64 execution test.

## Resolution

The explicit development shell and its pinned Rust tools are implemented together
with the broader self-contained environment in Issue 0013. Clippy is also exposed as
an independent flake check.
