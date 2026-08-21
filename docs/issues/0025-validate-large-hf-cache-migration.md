---
status: open
priority: P1
related_adrs:
  - ADR-0001
  - ADR-0008
created: 2026-08-21
updated: 2026-08-21
---
# Issue 0025: Validate large Hugging Face cache migration

- Status: Open
- Priority: P1
- Related ADR: ADR-0001, ADR-0008

## Objective

Validate migration of a representative large, sharded GX10 Hugging Face cache without
redownload or archive dependence on cache symlinks.

## Problem

The importer is fixture-tested, but the MVP requires an existing hundreds-of-GB cache
to be imported and redownloaded from ModelKeep into an empty client cache.

## Scope

- Inventory a representative source cache without recording credentials.
- Import, audit, disconnect upstream, and download the immutable commit to an empty
  client cache.
- Measure space requirements and preserve a repeatable operator record.

## Acceptance criteria

- All imported files are materialized and pass size/SHA-256 audit.
- Offline download succeeds after the source client cache is unavailable.
- No source cache symlink or blob path remains a durable dependency.

## Verification

```sh
modelkeep import-hf-cache <gx10-cache-path> /data
modelkeep audit /data
HF_ENDPOINT=<modelkeep-tailnet-url> hf download <repo> --revision <commit>
```

Run with upstream blocked and an empty destination cache. Retain the model commit,
byte totals, audit result, and offline download result.

## Risks and assumptions

This requires representative hardware and substantial temporary capacity; do not
delete the source cache until the complete acceptance record passes.
