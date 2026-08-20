# ADR-0008: Publish only complete upstream snapshots

- Status: Accepted
- Date: 2026-08-21

## Context

The pull-through HTTP surface may request one file at a time, while a model revision
usually consists of a complete repository snapshot. Publishing the requested file as
the revision would make later requests mistake a partial acquisition for a complete
archive, especially for multi-shard models.

## Decision

For the MVP, every pull-through acquisition downloads the complete upstream snapshot
before publishing the immutable revision.

- The upstream helper receives no file filter for pull-through acquisitions.
- The staging output must contain at least one safe file and a resolved commit.
- The manifest records `complete: true` only for a successful complete snapshot.
- A revision without `complete: true` is not a completed object and is not served.
- A complete revision is published once and remains immutable.
- Existing old-format or partial revisions are not silently upgraded or overwritten;
  they must be handled by an explicit administrative migration or deletion.

The per-file acquisition model is deferred until its completeness and crash-recovery
semantics can be specified separately.

## Rationale

Complete snapshot publication gives the archive a simple and auditable invariant:
the presence of a published revision means the revision is complete. It also keeps
the ordinary-file representation and immutable revision semantics from ADR-0001 and
ADR-0002 intact.

## Alternatives considered

- Publish only requested files: rejected because a revision directory could become a
  misleading partial archive.
- Append missing files to a published revision: rejected because published revisions
  are immutable.
- Add a mutable per-file state database: deferred because metadata must not become the
  authority for model bytes.

## Consequences

Cold misses use more bandwidth and temporary storage than a single-file fetch. Warm
hits are simple and reliable, and all files in a published revision remain available
without upstream access. Existing revisions created before this decision need an
explicit operator migration or removal plan.

## Validation

Tests cover complete manifests, refusal to serve incomplete manifests, multi-file cold
misses, concurrent requests, and interruption before publication.
