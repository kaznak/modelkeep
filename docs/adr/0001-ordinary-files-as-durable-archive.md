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

## Future storage-efficiency evaluation

As of 2026-08-22, ModelKeep intentionally does not add archive deduplication or delta
storage. This remains a worthwhile future experiment if real multi-revision model
archives show enough recoverable duplication to justify the added durability and
recovery complexity.

Candidate approaches, in increasing architectural impact, include:

- whole-file identity sharing with hardlinks or filesystem reflinks for unchanged
  shards while retaining ordinary materialized revision files;
- transparent filesystem-level block deduplication where the QNAP storage stack
  supports it safely and backup/restore preserves the required semantics;
- backup-layer content-defined chunking with established tools such as Borg or restic,
  which can reduce backup generations without changing the primary archive format;
- application-level content-defined chunking using a published algorithm such as
  FastCDC or Xet's GearHash-based chunking, after measuring its effectiveness on
  representative full fine-tunes, quantizations, reshards, and adapter-based models.

Before selecting an approach, a read-only experiment should measure physical bytes,
whole-file identity, and fixed-size/content-defined chunk reuse across representative
revisions. It must also account for chunk-index memory, integrity audit cost, Range
serving, atomic publication, explicit deletion, snapshots, backup, and bare-filesystem
restore.

Application-level chunks must not become the sole durable representation under this
ADR. Adopting a chunk store, on-demand reconstruction, or another opaque byte authority
requires a new ADR that supersedes this decision and defines a migration and recovery
path requiring no upstream redownload.
