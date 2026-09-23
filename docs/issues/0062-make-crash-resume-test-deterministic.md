---
status: in_progress
priority: P1
related_adrs:
  - ADR-0009
  - ADR-0017
created: 2026-09-23
updated: 2026-09-23
---
# Issue 0062: Make the crash/resume test deterministic

- Status: In progress
- Priority: P1
- Related ADR: ADR-0009, ADR-0017

## Objective

Remove the scheduling race in the black-box crash/resume check so that it kills
ModelKeep only after both the partial payload and its resolved commit identity have
crossed the durable staging boundary.

## Problem

The test currently observes `partial.bin` and immediately sends SIGKILL. The helper
prints its resolved commit before writing that file, but ModelKeep consumes helper
stdout asynchronously. A fast test process can therefore kill ModelKeep before it
has atomically persisted `resolved_commit` in `.modelkeep-fetch.json`. Recovery then
correctly discards the commit-less staging directory and the resume helper returns a
502, making the test depend on process scheduling.

## Acceptance criteria

- The crash fixture makes the payload observable before releasing its resolved event,
  proving that payload existence alone is not used as the kill boundary.
- The test waits for the expected payload and persisted resolved commit before SIGKILL.
- Timeout failures report which durable checkpoint was missing.
- The focused archive crash/upgrade check and the full required validation pass.
- Native amd64 and arm64 release jobs pass before publishing the next patch release.

## Verification

```sh
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
nix build .#checks.x86_64-linux.archive-crash-upgrade --no-link
nix flake check
```

## Risks and assumptions

`.modelkeep-fetch.json` is an internal durable test boundary written with file and
directory synchronization. This issue changes only the black-box test fixture and
harness; it does not change archive or resume semantics.
