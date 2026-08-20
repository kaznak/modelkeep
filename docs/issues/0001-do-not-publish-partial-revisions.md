---
status: open
priority: P0
related_adrs:
  - ADR-0001
  - ADR-0002
  - ADR-0005
  - ADR-0008
created: 2026-08-21
updated: 2026-08-21
---
# Issue 0001: Do not publish partial revisions

- Status: Open
- Priority: P0
- Related ADR: ADR-0001, ADR-0002, ADR-0005

## Objective

Ensure that a revision is not marked complete when only one requested file or shard
has been acquired.

## Problem

The pull-through path can request one file at a time. If that file is published as a
revision, later requests may see the revision directory and treat the revision as a
complete archive even though other model files are missing. This is especially risky
for multi-shard models.

## Scope

Choose and implement one of these explicit models:

1. acquire and validate the complete upstream snapshot before publishing the revision;
2. keep revision and per-file acquisition state separate, publishing only files that
   are independently complete and fetching missing files on demand.

The selected model must preserve immutable published bytes and must not expose partial
data as a completed object.

## Acceptance criteria

- A request for one file cannot make an incomplete revision appear complete.
- Concurrent requests for different files of the same revision are correct.
- A failure during a multi-file acquisition leaves no completed revision behind.
- A warm hit works offline after the required archive content is complete.
- Tests cover a multi-shard or multi-file cold miss and interruption before completion.

## Verification

```sh
cargo test --all-features
```

Also run a black-box client test that requests at least two files concurrently and
checks that both are available after the acquisition completes.

## Risks and assumptions

Fetching complete snapshots may use more temporary disk space and bandwidth. Per-file
publication may be more complex and requires an explicit completeness model.
