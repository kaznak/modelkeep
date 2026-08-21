---
status: open
priority: P0
related_adrs:
  - ADR-0001
  - ADR-0008
  - ADR-0010
created: 2026-08-21
updated: 2026-08-21
---
# Issue 0037: Exclude upstream helper metadata from published revisions

- Status: Open
- Priority: P0
- Related ADR: ADR-0001, ADR-0008, ADR-0010

## Objective

Publish only validated model files from official Hugging Face fetch staging and serve
only files named by the completed revision manifest.

## Problem

`snapshot_download(local_dir=...)` creates `.cache/huggingface` metadata. The helper
omits it from its returned file list, but staging reuse renames the whole directory
into the durable revision. A freshly fetched revision therefore fails `audit`, and
manifest-external files can be resolved over HTTP.

## Acceptance criteria

- Official-fetch metadata is absent from the published revision.
- A revision produced from staging containing `.cache/huggingface` passes verification.
- Files absent from the manifest cannot be resolved or served.
- Staging reuse and atomic publication remain intact.

## Verification

```sh
python3 -m unittest -v upstream/test_hf_fetch.py
cargo test --all-features
nix flake check
```

Add regression tests for helper metadata cleanup, published file sets, and
manifest-external file rejection.

## Risks and assumptions

`.cache` may be a legitimate repository path. Cleanup must target metadata created by
the official client, while the manifest remains the authority for what ModelKeep may
serve.
