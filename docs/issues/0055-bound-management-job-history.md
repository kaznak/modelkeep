---
status: open
priority: P2
related_adrs:
  - ADR-0015
created: 2026-08-24
updated: 2026-08-24
---
# Issue 0055: Bound management job history resource usage

- Status: Open
- Priority: P2
- Related ADR: ADR-0015

## Objective

Prevent long-running installations from loading and retaining an unbounded number of
completed management jobs while keeping useful operator history.

## Problem

Every job is stored as a separate JSON file and `JobManager::open` reads every record
into one in-memory `BTreeMap`. The API displays only recent entries, but there is no
retention, compaction, or indexed bounded-load strategy. Scheduled audits, refreshes,
or prefetches can therefore make startup time, memory use, and metadata inode usage
grow without bound.

## Scope

- Define a management-job retention policy distinct from model revision retention.
- Bound startup memory and latency while preserving active jobs and a useful recent
  history.
- Provide an operator-visible way to understand or configure the bound.
- Ensure cleanup is crash-safe and never traverses or deletes model archive content.

## Acceptance criteria

- Startup resource use is bounded with a large synthetic job history.
- Active/interrupted jobs and the configured recent history remain queryable in a
  deterministic order.
- Retention affects only management metadata under `state/jobs` and cannot delete
  revisions, refs, manifests, or model files.
- Cleanup interruption leaves readable job state and can be retried safely.

## Verification

```sh
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
nix flake check
```

Include large-history startup, retention-boundary, interrupted-cleanup, and archive
isolation tests.

## Risks and assumptions

Job history is reconstructible operational metadata, not durable model data. Its
retention policy must not be confused with the prohibition on automatic archive GC.
