# ADR-0001: Store durable model data as ordinary materialized files

- Status: Accepted
- Date: 2026-08-21

## Context

ModelKeep exists to preserve model revisions independently of transient client caches and independently of a particular mirror implementation version. The archive may contain hundreds of gigabytes or multiple terabytes, so a server upgrade must not require rebuilding or redownloading the archive.

Systems such as ModelArk demonstrate the value of treating preservation as a separate concern from ordinary caching. ModelKeep, however, must also serve archived bytes directly through an HF-compatible HTTP interface.

## Decision

Store each archived revision as ordinary materialized repository files beneath an immutable commit-specific directory. Do not make an opaque application cache, database blob store, compressed archive, or version-specific cache layout the sole durable representation.

Indexes, manifests, and SQLite metadata may accelerate operation, but model bytes remain independently readable from the filesystem.

## Rationale

This keeps archived data usable if ModelKeep itself is replaced. It also makes HTTP Range serving straightforward and avoids coupling multi-terabyte durable state to application cache migrations.

## Alternatives considered

- Hugging Face local cache layout as the durable archive: rejected because it is fundamentally a cache and can be pruned or evolve independently of ModelKeep's preservation guarantees.
- Olah-style private cache format: rejected as the durable representation because cache-format migration must not endanger archived data.
- ModelArk/git-annex storage: useful for offline/multi-drive archival, but unnecessary for the initial QNAP storage model and not directly optimized for transparent HTTP serving.
- Transparent compression such as ZipNN: deferred because byte-range serving of original files becomes more complex.

## Consequences

The archive consumes approximately the original materialized model size and may duplicate identical blobs across revisions. Deduplication may be considered later only if it preserves the ordinary-file durability contract.

The filesystem layout becomes part of the durable compatibility surface and must change only through an explicit migration/ADR.

## Validation

An archived revision must remain directly inspectable and serveable after deleting/rebuilding all derived metadata and replacing the ModelKeep executable.
