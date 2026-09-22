---
status: in_progress
priority: P1
related_adrs:
  - ADR-0005
  - ADR-0008
created: 2026-09-22
updated: 2026-09-22
---
# Issue 0061: Fetch cold revisions requested through the tree API

- Status: In Progress
- Priority: P1
- Related ADR: ADR-0005, ADR-0008

## Objective

Allow a supported Hugging Face client to cold-download an immutable revision when
its first request is the repository tree API.

## Problem

QNAP acceptance with `huggingface_hub` 1.27.0 requests
`/api/models/<repo>/tree/<commit>` before any model-info or resolve request. The tree
handler currently reads only the archive and returns 404 for a cold revision, while
the other handlers initiate pull-through acquisition.

## Acceptance criteria

- A cold tree request initiates one complete snapshot acquisition and returns the
  archived tree.
- Immutable commits and mutable refs use the same acquisition/error semantics as
  model-info requests.
- Warm and upstream-offline tree requests remain archive-only successes.
- A regression test covers tree-first cold acquisition.

## Verification

```sh
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
nix flake check
```

Re-run the QNAP acceptance cold phase with the supported real HF client.

## Risks and assumptions

The complete-snapshot policy remains unchanged; a tree miss fetches and atomically
publishes the complete revision before returning metadata.
