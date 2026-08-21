# ADR-0010: Reuse validated fetch staging for publication

- Status: Accepted
- Date: 2026-08-21

## Context

A pull-through fetch already materializes a complete snapshot under the archive temporary directory. Copying that snapshot into another staging tree temporarily doubles storage and performs an unnecessary full read/write pass.

## Decision

When a source snapshot is inside ModelKeep archive temporary directory, publication validates the listed files and writes the manifest in place, then atomically renames that staging directory into the immutable revision path. Source directories outside the archive continue to use the copying import path.

The source files are still validated for safe paths, containment, regular-file status, size, and SHA-256 before publication. The lease marker is removed immediately before the final rename.

Files and directories not named by the validated source-file list are removed from
reused staging before the manifest is written. This excludes downloader metadata and
other helper artifacts from the durable revision. The completed manifest is also the
allow-list for file resolution, so an unlisted file is never served even if archive
storage is modified after publication.

## Consequences

- Normal cold pull-through requires one materialized payload copy plus metadata and bounded temporary overhead.
- Atomic rename remains the publication boundary on a single filesystem.
- External Hugging Face cache imports retain their existing materialization behavior.
- The archive temporary directory must share a filesystem with published revisions.
