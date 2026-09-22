---
status: open
priority: P2
related_adrs:
  - ADR-0015
created: 2026-09-22
updated: 2026-09-22
---
# Issue 0059: Deduplicate active management jobs

- Status: Open
- Priority: P2
- Related ADR: ADR-0015

## Objective

Return an existing active management job instead of creating another job for the
same operation target.

## Problem

Different submissions can currently create multiple queued or running job records
for the same operation, repository, and revision. Pull-through single-flight avoids
duplicating the upstream acquisition, but only the leader job receives progress.
Follower jobs therefore remain at `acquiring_snapshot` with unknown progress until
the shared operation finishes, making the management UI misleading.

Idempotency keys prevent replay of one HTTP request. They do not deduplicate
semantically equivalent requests submitted with different keys or by different
authorized principals.

## Scope

- Define an operation key from job kind, repository ID, and revision.
- During submission, return the matching queued or running job rather than creating
  and spawning another one.
- Apply the check atomically with job creation so concurrent requests cannot create
  duplicate active jobs.
- Preserve the original job principal and durable job identity.
- Continue allowing a new job after the previous matching job has completed, failed,
  or been cancelled.
- Keep different operations or revisions independent.
- Preserve existing idempotency-key conflict behavior.

## Acceptance criteria

- Sequential equivalent submissions with different idempotency keys return the same
  active job ID.
- Concurrent equivalent submissions create and execute exactly one job.
- Equivalent submissions from different authorized principals return the same active
  job without replacing its recorded principal.
- Completed, failed, cancelled, and interrupted jobs do not prevent a retry.
- Different job kinds, repositories, or revisions remain distinct.
- The API distinguishes a newly accepted job from a reused active job using its
  existing `202 Accepted` versus `200 OK` behavior.
- No additional upstream fetch is started for a reused job.

## Verification

```sh
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
nix flake check
```

Add focused management API tests for sequential, concurrent, cross-principal,
terminal-state retry, and different-target submissions.

## Risks and assumptions

- Deduplication is process-local and reconstructed from durable job records at
  startup; ModelKeep currently supports one management process per archive.
- Prefetch and refresh for the same repository and revision remain different
  operations because their ref semantics differ.
- Audit has no repository or revision, so only one active audit job should exist.
