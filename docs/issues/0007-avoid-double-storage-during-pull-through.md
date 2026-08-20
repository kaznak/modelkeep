---
status: open
priority: P1
related_adrs:
  - ADR-0001
  - ADR-0002
  - ADR-0005
created: 2026-08-21
updated: 2026-08-21
---

# Issue 0007: Avoid double storage during pull-through

- Status: Open
- Priority: P1
- Related ADR: ADR-0001, ADR-0002, ADR-0005

## Objective

Make cold pull-through publication require one materialized model copy plus bounded metadata/temporary overhead rather than approximately two complete copies.

## Problem

The upstream helper can materialize a complete model in fetch staging and the publication path can then copy the same payload into a second revision staging tree before the first is removed. For models tens or hundreds of gigabytes in size, this can nearly double temporary capacity requirements and add a full NAS read/write pass.

## Scope

* Reuse validated fetch staging as publication staging where possible.
* Validate paths, sizes, hashes, and manifest state before publication.
* Keep staging and final revision on the same filesystem so final publication can use atomic rename.
* Preserve a separate import path where Hugging Face cache symlinks/blobs require materialization.

## Acceptance criteria

* A normal N-byte cold pull does not retain another N-byte payload copy simultaneously.
* SHA-256/size validation remains in place.
* Publication remains atomic.
* Interrupted publication never exposes an incomplete revision.
* Existing published revisions remain immutable.
* `import-hf-cache` remains correct.

## Verification

    cargo test --all-features
    cargo clippy --all-targets --all-features -- -D warnings

Use a generated large fixture or equivalent instrumentation to demonstrate that two complete payload copies are not simultaneously retained. Test interruption immediately before and after the final rename.

## Risks and assumptions

Atomic rename requires source and destination to be on the same filesystem. Optimizing pull-through must not weaken import correctness or durable publication.
