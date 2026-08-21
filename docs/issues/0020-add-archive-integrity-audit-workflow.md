---
status: open
priority: P2
related_adrs:
  - ADR-0001
  - ADR-0006
created: 2026-08-21
updated: 2026-08-21
---
# Issue 0020: Add an archive integrity audit workflow

- Status: Open
- Priority: P2
- Related ADR: ADR-0001, ADR-0006

## Objective

Let operators audit all published revisions and detect corruption or malformed durable
state without relying on a database or contacting upstream.

## Problem

`modelkeep verify` checks one known repository and commit. There is no supported way
to enumerate the entire archive, verify every manifest and file, produce a machine-
readable result, or resume a long-running audit on a multi-terabyte QNAP share.

## Scope

- Enumerate repositories and completed revisions from durable filesystem state.
- Verify manifest completeness, file size, SHA-256, safe paths, and unexpected files.
- Produce structured progress and a non-zero exit status for any failed revision.
- Keep the audit read-only and bounded in memory; do not repair or delete data.
- Document scheduling with QNAP while limiting I/O impact on model serving.

## Acceptance criteria

- A full audit works without SQLite and without upstream network access.
- Corruption, missing files, malformed manifests, and unsafe paths are reported with
  repository and commit identity.
- Healthy published data is never modified.
- Operators can distinguish an interrupted audit from a successful clean audit.

## Verification

```sh
nix develop -c cargo test --all-features
nix develop -c cargo clippy --all-targets --all-features -- -D warnings
nix flake check
```

Test healthy archives and injected size, digest, missing-file, and malformed-manifest
failures using filesystem fixtures.

## Risks and assumptions

Hashing multi-terabyte archives is I/O intensive. Scheduling and optional throttling
must not weaken verification or make partial results look complete.
