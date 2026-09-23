---
status: in-progress
priority: P0
related_adrs:
  - ADR-0008
  - ADR-0009
  - ADR-0017
created: 2026-09-23
updated: 2026-09-23
---
# Issue 0068: Audit production-boundary test coverage

- Status: In Progress
- Priority: P0
- Related ADR: ADR-0008, ADR-0009, ADR-0017

## Objective

Prevent simple production acquisition and restart regressions from escaping unit and
synthetic-fixture tests by exercising the composed boundaries shipped in the image.

## Problem

Recent failures passed existing tests because each layer was tested separately:

- helper JSON parsing used synthetic shell output;
- helper acquisition tests mocked the official client;
- crash/restart tests replaced the production helper with synthetic scripts;
- real supported-client tests covered cold and warm operation but not interruption;
- progress tests asserted completed-file counters rather than observable partial-byte
  behavior.

Those tests prove useful local contracts but not their production composition. A
legacy internal staging entry and resume-only client stdout both reached released
images before a black-box test exercised the exact boundary.

## Write scope

- test-coverage map and ownership rules;
- deterministic Nix checks that compose the production helper, supported clients,
  restart/recovery, legacy archive state, and progress reporting where applicable;
- focused production code changes only when a newly added regression test exposes an
  incorrect behavior.

## Do not touch

- archive representation, immutability, deletion policy, or deployment credentials;
- assertions merely to make current behavior pass;
- live deployment endpoints or site-specific evidence.

## Acceptance criteria

- Every production acquisition boundary has an owning black-box test, not only a
  synthetic or mocked test.
- Crash/restart CI executes retained partial staging through the production helper
  event channel and proves atomic publication and staging cleanup.
- Supported real HF client versions cover cold, warm/offline, and production-helper
  acquisition without mirror bypass.
- Legacy durable metadata is exercised through a fresh real client.
- Progress tests distinguish byte movement, heartbeat/activity, and completed files;
  a constant byte count cannot be presented as fresh transfer progress.
- A documented matrix identifies which checks are deterministic CI coverage and which
  assertions remain filesystem/runtime-specific QNAP acceptance.

## Verification

```sh
nix develop -c cargo fmt --check
nix develop -c cargo clippy --all-targets --all-features -- -D warnings
nix develop -c cargo test --all-features
nix flake check
```

Review each regression against the built helper and supported-client derivations; do
not count two fixtures that independently reproduce the same assumed contract as
cross-boundary coverage.

## Risks and assumptions

Fully online Hub tests are nondeterministic and unsuitable as the only CI gate. Use
supported real clients against deterministic local protocol fixtures, while retaining
small optional real-upstream observations separately. QNAP acceptance should validate
filesystem and container-runtime behavior, not compensate for missing deterministic
application tests.
