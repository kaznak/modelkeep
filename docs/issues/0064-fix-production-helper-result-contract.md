---
status: in-progress
priority: P0
related_adrs:
  - ADR-0008
  - ADR-0017
created: 2026-09-23
updated: 2026-09-23
---
# Issue 0064: Fix production helper result contract failure

- Status: In Progress
- Priority: P0
- Related ADR: ADR-0008, ADR-0017

## Objective

Restore new model acquisition in the released OCI image while retaining strict,
credential-safe validation of the upstream helper event contract.

## Problem

In version 0.4.4, both management prefetch and pull-through cold misses can fail after
snapshot inventory but before payload progress. Management reports
`error_class: upstream` and `upstream invalid output: helper emitted a malformed result
event`. The failure reproduces with a small public test repository, so acquisition is
not reliably usable even though completed archived revisions remain readable.

This issue deliberately contains no deployment hostname, network identity, operator
identity, private configuration, or site-specific archive inventory.

## Write scope

- `upstream/hf_fetch.py` and its focused tests;
- the Rust helper event parser and focused tests only if the emitted contract is valid
  but parsed incorrectly;
- Nix checks required to exercise the actual packaged helper/client combination;
- operational documentation only where failure diagnostics change.

## Do not touch

- archive representation, published revisions, or deletion policy;
- authentication and deployment-specific configuration;
- resumable-staging semantics except where needed to preserve the existing contract;
- unrelated client protocol behavior.

## Acceptance criteria

- The packaged helper's exact stdout event stream for a supported real
  `huggingface_hub` client is accepted by ModelKeep.
- A small public model completes through both management prefetch and pull-through
  cold-miss paths, then remains available as a warm hit.
- Malformed, unsupported, or credential-bearing helper output is still rejected
  without copying raw output into logs or management state.
- The regression is covered by a test using the actual helper rather than only a
  synthetic fixture that independently reimplements its event stream.
- Existing resumable acquisition, integrity, and HF client integration tests pass.

## Verification

```sh
python3 -m unittest -v upstream/test_hf_fetch.py
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
nix flake check
```

Also run the packaged ModelKeep/helper combination against a small public repository
and retain only sanitized pass/fail evidence. Do not commit raw production logs or
deployment endpoints.

## Risks and assumptions

The helper must keep stdout machine-readable and credential-free; diagnostics belong
on stderr and must not be surfaced verbatim. A fix that merely accepts arbitrary JSON
would hide contract drift and weaken the trust boundary.
