# ADR-0019: Separate model and dataset archive namespaces

- Status: Accepted
- Date: 2026-09-23

## Context

Hugging Face permits a model repository and a dataset repository to have the same
repository ID. ModelKeep's durable archive has historically treated every repository
as a model and stores it below `<archive-root>/models`. Reusing that namespace for
datasets would make the two repository types collide and could serve bytes from the
wrong repository.

The existing model layout is already durable state on QNAP. Adding dataset support
must not move it, rewrite it, or require any archived model to be downloaded again.
Repository type also crosses untrusted HTTP, management, helper, staging, and
manifest boundaries, so it cannot be accepted as an arbitrary path component.

## Decision

Keep the existing model namespace unchanged and add a sibling dataset namespace:

```text
<archive-root>/models/<namespace>/<name>/...
<archive-root>/datasets/<namespace>/<name>/...
```

In the production container these are `/data/models` and `/data/datasets`. Revisions
remain ordinary materialized files in immutable commit-specific directories in both
namespaces. Mutable refs remain separate from revisions, publication still requires
a complete validated snapshot and atomic rename, and indexes remain reconstructible
from the archive filesystem and manifests.

Repository type is a closed enum with exactly `model` and `dataset`. All external
inputs are parsed through this allowlist before selecting an archive namespace;
unknown values are rejected and are never used as filesystem path components.

Every newly written manifest records `repo_type`. The recorded type must match the
namespace and the type requested by an operation. A mismatch is an integrity error,
not a cache miss. For compatibility, a legacy manifest without `repo_type` is treated
as `model` only when it is below the existing `models` namespace; it cannot authorize
content below `datasets`.

Writable archive initialization creates `datasets` if it is absent. This is the only
upgrade needed: existing `models` contents and paths remain unchanged. Read-only open
continues to accept an existing legacy archive that has `models` but no `datasets`,
and must not create the missing directory. Dataset lookup in such an archive is an
ordinary not-found result until a writable initialization creates the namespace.

A downgrade to a binary predating this decision continues to see the unchanged
`models` namespace and ignores the sibling `datasets` namespace. It therefore cannot
serve dataset bytes as model bytes. Dataset archives remain on disk untouched but
are unavailable through the older binary until ModelKeep is upgraded again.

## Rationale

Separate top-level namespaces reflect Hugging Face repository identity while
preserving the established model archive byte-for-byte. They make collisions
impossible without introducing a migration, an opaque catalog, or a database as the
authority for repository type.

Recording and checking the type in each manifest provides local evidence that a
revision was published into the correct namespace. Keeping the type enum closed also
preserves path-safety and forces any future repository kind to receive an explicit
layout decision.

## Alternatives considered

- Move models to `<archive-root>/repositories/models`: rejected because changing
  durable paths would create an unnecessary multi-terabyte migration and complicate
  downgrade and restore.
- Store both types below `models` and add a type component deeper in the tree:
  rejected because it changes every existing model path or requires ambiguous
  special cases.
- Distinguish types only in a database or index: rejected because repository identity
  must remain reconstructible from durable archive state.
- Accept arbitrary upstream repository-type strings as directory names: rejected
  because it expands the filesystem trust boundary and makes future layout semantics
  accidental.

## Consequences

- A model and dataset with the same repository ID can coexist without collision.
- Existing model archives require no relocation, rewrite, or upstream download.
- Backup, restore, audit, inventory, deletion, import, and management operations must
  preserve and validate repository type.
- Operators must include both `models` and `datasets` when backing up the archive
  root. Existing whole-`/data` backups already do so after the new directory exists.
- Older binaries cannot use dataset archives, but can safely operate on existing
  models without interpreting datasets as models.
- Supporting another Hugging Face repository kind requires extending the enum and an
  explicit decision for its durable namespace.

## Validation

- Publish model and dataset fixtures with the same repository ID and verify their
  files, refs, manifests, inventory, and HTTP responses remain distinct.
- Reject unknown repository types and manifests whose type does not match their
  namespace or requested operation.
- Open a legacy model-only archive read-only and verify that no filesystem entry is
  created and existing models remain readable.
- Upgrade a legacy archive through writable initialization and verify that only the
  empty `datasets` namespace is added.
- Exercise cold, warm, and upstream-blocked dataset downloads with each supported
  real Hugging Face client.
- Verify a pre-dataset binary can still open and serve the unchanged model namespace
  while leaving dataset files untouched.
