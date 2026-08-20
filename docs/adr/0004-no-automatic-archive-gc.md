# ADR-0004: No automatic garbage collection of archived revisions

- Status: Accepted
- Date: 2026-08-21

## Context

ModelKeep's primary distinction from a cache is preservation. Automatic eviction based on age, access frequency, or storage pressure could delete the only retained copy of a model that has disappeared upstream.

## Decision

ModelKeep does not automatically delete completed archived revisions.

Deletion requires an explicit administrative action. Disk pressure is reported as an operational error/alert rather than resolved by silently evicting archive contents.

QNAP snapshots/backups are complementary protection against operator error and storage failure.

## Rationale

Predictable preservation is more important than autonomous capacity management for the initial system.

## Alternatives considered

- LRU/size-based GC: rejected because it makes archive survival probabilistic.
- Automatically delete revisions still available upstream: rejected because upstream availability can change after deletion.

## Consequences

Operators must provision and monitor storage capacity. Administrative deletion tooling must be explicit and should eventually support dry-run and provenance reporting.

## Validation

Disk-full conditions do not cause ModelKeep to delete existing completed revisions.
