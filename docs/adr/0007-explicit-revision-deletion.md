# ADR-0007: Explicit deletion of unreferenced revisions

- Status: Accepted
- Date: 2026-08-21

## Context

ModelKeep deliberately keeps completed revisions and does not perform automatic
garbage collection. Operators still need a controlled way to reclaim storage when
they have verified that a revision is no longer needed.

Deletion is destructive: a revision may be the only durable copy of a model, and a
mutable ref may still resolve to it.

## Decision

Provide explicit administrative deletion for one commit revision at a time.

- Deletion is never automatic and is never triggered by disk pressure.
- A revision referenced by any mutable ref in the repository cannot be deleted.
- The command supports a dry-run that performs all validation without changing data.
- Deletion operates only on a validated repository ID and commit ID; arbitrary paths
  are not accepted.
- A successful deletion removes the complete immutable revision directory and fsyncs
  its parent directory.
- Deletion does not remove or rewrite other revisions, refs, manifests, or metadata.
- Concurrent HTTP requests are not redirected to upstream as a consequence of
  deletion; a later request may perform the normal cache-miss behavior if configured.

The initial CLI surface is:

```text
modelkeep remove [archive-root] <repo-id> <commit> [--dry-run]
```

## Rationale

Commit-level deletion is easy to audit and keeps the durable archive model explicit.
Ref protection prevents the normal serving path from losing the revision selected by
`main` or another mutable reference. Dry-run makes operator review possible before a
destructive action.

## Alternatives considered

- Automatic LRU or size-based GC: rejected by ADR-0004 because preservation is the
  primary purpose of ModelKeep.
- Deleting revisions still available upstream: rejected because upstream availability
  can change after deletion.
- Allowing deletion of referenced revisions: rejected because it creates a broken
  mutable ref and an avoidable cache miss.
- Deleting arbitrary filesystem paths: rejected because repository and path input is
  untrusted.

## Consequences

Operators remain responsible for deciding which unreferenced revisions are safe to
remove. A revision can be retained deliberately by keeping a ref to it. The command
does not reclaim duplicate files across revisions and does not implement automatic
GC.

## Validation

Tests must prove that dry-run preserves the revision, unreferenced deletion removes
the revision, and referenced deletion is rejected while the revision remains
serveable.
