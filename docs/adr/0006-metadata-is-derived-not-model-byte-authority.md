# ADR-0006: Metadata indexes are derived state, not authority for model bytes

- Status: Accepted
- Date: 2026-08-21

## Context

SQLite or another index is useful for lookup, status, provenance, and operations, but making it the only description of archived data creates a fragile single point of failure.

## Decision

Model bytes and commit-specific filesystem state are the durable authority. Database/index metadata must be reconstructible where practical from archive manifests/filesystem state.

A database failure must not imply that archived model bytes need to be redownloaded.

## Rationale

This follows the principle that the server implementation and its indexes are replaceable while archived model data is durable.

## Alternatives considered

- Database as the sole catalog and object authority: rejected because database loss/corruption would unnecessarily endanger usability of otherwise intact archive data.

## Consequences

Archive manifests may duplicate some index information. Recovery/reindex tooling is required before production maturity.

## Validation

A test/recovery procedure can rebuild sufficient metadata from an intact archive to serve an archived revision.
