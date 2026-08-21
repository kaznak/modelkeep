---
status: done
priority: P1
related_adrs: []
created: 2026-08-21
updated: 2026-08-21
---
# Issue 0013: Make the development environment self-contained

- Status: Done
- Priority: P1
- Related ADR: None

## Objective

Provide a pinned Nix development environment and validation outputs so a developer
can build, format, lint, test, and run the supported Hugging Face client tooling
without relying on Rust, Python, or other development tools installed by the host or
the user's profile.

## Problem

The flake currently defines reproducible packages and an OCI image, but no explicit
development shell. `nix develop` therefore derives an environment from the default
package. It supplies Cargo and Rustc while resolving tools such as Rustfmt, Clippy,
Python, `hf`, and Git from the host when they happen to be installed. This can mix
incompatible Rust toolchain versions, omit the supported Hugging Face client, and
make documented validation depend on a developer's machine.

The flake also exposes only the ModelKeep package as a check. Formatting, Clippy,
and supported-client integration validation are not represented as explicit flake
checks.

## Write scope

- `flake.nix` and `flake.lock` if input changes are required
- this issue record and development documentation
- repository-owned test definitions needed to expose existing validation through
  flake checks

## Do not touch

- archive representation, publication, and recovery semantics
- HTTP compatibility behavior or upstream acquisition policy
- accepted ADRs
- application dependencies unrelated to development or validation
- host-level Nix, Home Manager, AppArmor, or user-profile configuration

## Acceptance criteria

- The flake exposes `devShells.default` for every supported system.
- Cargo, Rustc, Rustfmt, and Clippy come from one compatible toolchain pinned by
  `flake.lock`.
- Python and the supported `huggingface_hub` / `hf` client are available from the
  pinned Nix environment, together with the CA certificates and helper tools needed
  by repository-owned tests.
- Every tool required by documented build and validation commands resolves from the
  Nix store when the host environment is excluded. No required tool is taken from
  `/usr`, `/bin`, a user profile, or a language-specific user installation.
- `nix develop` supports formatting, Clippy, unit tests, and the repository's
  supported Hugging Face client integration tests without requiring separately
  installed Rust or Python tooling.
- Flake checks expose distinct formatting, Clippy, test, and supported-client
  integration validation where those suites exist. The package and OCI image remain
  reproducible from the pinned inputs.
- The flake continues to evaluate for both `x86_64-linux` and `aarch64-linux`.
- README development instructions use the Nix environment as the supported workflow
  and state that Nix plus the underlying Linux execution environment are the host
  prerequisites.

## Verification

```sh
nix develop --ignore-environment -c sh -c '
  set -eu
  for tool in cargo rustc rustfmt cargo-clippy python3 hf git; do
    path=$(command -v "$tool")
    case "$path" in /nix/store/*) ;; *) echo "$tool resolved outside the Nix store: $path" >&2; exit 1;; esac
  done
  python3 -c "import huggingface_hub"
'
nix develop --ignore-environment -c cargo fmt --check
nix develop --ignore-environment -c cargo clippy --all-targets --all-features -- -D warnings
nix develop --ignore-environment -c cargo test --all-features
nix flake check
nix flake show --all-systems
```

Run the repository's supported Hugging Face client integration suite through its
flake check once that check is exposed. Cross-system evaluation does not replace an
`aarch64-linux` execution test.

## Dependencies

Issue 0012 covers the immediate Rustfmt and Clippy mismatch and may be completed as
part of this issue. This issue is broader: it also owns the Python/Hugging Face client
environment, host-independence verification, explicit flake checks, and development
documentation.

## Risks and assumptions

- Nix, the Nix daemon, the Linux kernel, and initial access to configured Nix
  substituters remain host prerequisites; self-contained does not mean independent
  of the execution platform or initial dependency acquisition.
- Tests that intentionally exercise upstream Hugging Face still require network
  access and must remain distinguishable from offline unit checks.
- Native `aarch64-linux` execution may require separate hardware or CI even though
  the output evaluates on an x86_64 development host.

## Resolution

The flake now provides a default development shell on both supported systems with a
single pinned Rust toolchain, Python and the official Hugging Face client, Git, and
CA certificates. Formatting, Clippy, unit tests, and the supported-client environment
are separate flake checks. There is not yet a repository-owned live Hugging Face
integration suite; when one is added, it must remain distinct from offline checks.
The GitHub Actions image matrix runs `nix flake check` natively on both its amd64 and
arm64 runners before building the corresponding OCI image.
