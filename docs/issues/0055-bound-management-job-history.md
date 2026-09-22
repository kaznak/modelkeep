---
status: open
priority: P2
related_adrs:
  - ADR-0015
created: 2026-08-24
updated: 2026-09-23
---
# Issue 0055: Page management job history without loading every record

- Status: Open
- Priority: P2
- Related ADR: ADR-0015

## Objective

Keep the existing per-job JSON history while preventing startup and steady-state
memory use from growing with the total number of completed management jobs.

## Problem

Every job is stored as a separate JSON file under `state/jobs`, and
`JobManager::open` currently reads every record into one in-memory `BTreeMap`. The API
is paginated, but the implementation has already loaded and retained the entire
history before serving the first page. Scheduled audits, refreshes, or prefetches can
therefore make startup time and memory use grow with historical job count even when
the UI needs only a small recent page.

The history itself is useful for audit and diagnosis. Automatic deletion is not
required to solve the loading problem and should not be introduced merely to reduce
UI output. For the foreseeable scale, operators may remove old terminal-job JSON
records manually while ModelKeep is stopped if disk or inode usage becomes material.

## Scope

- Keep per-job JSON files as the durable history representation; do not add SQLite as
  part of this issue.
- Load queued and running jobs at startup so they can be marked interrupted, but do
  not retain every terminal job in memory.
- Read terminal history from disk only as needed for a bounded API page, preserving
  the existing deterministic newest-first cursor semantics.
- Resolve `GET /jobs/{id}` directly from its validated JSON filename when the job is
  not active in memory.
- Keep active-job deduplication correct. Define a bounded, explicit idempotency-key
  lifetime or a small reconstructible file index so submission does not require all
  historical jobs to remain resident.
- Keep the UI page size bounded and make additional history an explicit pagination
  action.
- Document safe manual removal of terminal-job JSON while ModelKeep is stopped.

## Out of scope

- Automatic age- or count-based deletion of job history.
- SQLite or another database/index dependency.
- Compaction of job records into a different durable format.
- Any cleanup of revisions, refs, manifests, model files, or fetch staging.

## Acceptance criteria

- Startup does not deserialize or retain every terminal job with a large synthetic
  history; memory use is proportional to active jobs plus bounded working data.
- Queued and running records found after restart still become queryable failed jobs
  with phase `interrupted`.
- The first and subsequent API pages return the same deterministic newest-first
  ordering and cursor behavior as the current API while decoding only bounded page
  data.
- Direct lookup by a valid job ID works for both active and disk-only terminal jobs.
- Concurrent equivalent submissions still create or return one active job, and the
  chosen idempotency lifetime/index behavior has regression coverage.
- No automatic history deletion occurs, and no code path introduced by this issue can
  traverse or delete model archive content.

## Verification

```sh
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
nix flake check
```

Include large-history startup/load-count, multi-page ordering, direct disk lookup,
restart interruption, active deduplication/idempotency, malformed-record, and archive
isolation tests.

## Risks and assumptions

Filesystem directory enumeration may still be proportional to the number of JSON
files even when decoding and memory use are bounded. That is acceptable initially;
introduce a reconstructible index only after measurement demonstrates a need.

Job IDs currently contain a timestamp component, but paging must not depend on an
unvalidated filename or assume directory iteration order. Corrupt or malformed job
records must be reported safely without preventing active-job recovery where
practical.
