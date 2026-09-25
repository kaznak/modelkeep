---
status: open
priority: P3
related_adrs:
  - ADR-0012
created: 2026-09-26
updated: 2026-09-26
---
# Issue 0091: Answer repository info requested without a revision

- Status: Open
- Priority: P3
- Related ADR: ADR-0012

## Objective

Decide whether `GET /api/{models,datasets}/{namespace}/{repo}` is answered, and if so
with which revision.

## Problem

Reported from the GX10 deployment on 2026-09-26: the route answers `404` with an empty
body. Both pinned `huggingface_hub` versions request it from `model_info()` /
`repo_info()` when no revision is passed (`hf_api.py`, `path = .../api/models/{repo_id}
if revision is None`). `snapshot_download` and `hf download` pass a revision and are not
affected, and llama.cpp v0.4.1 does not use the route.

The Hub answers the repository's default branch, which ModelKeep does not record. The
decision is whether to answer `main`, as `refs` does (Issue 0088), or to keep `404`.

## Acceptance criteria

- `HfApi().model_info(repo_id)` with no revision either succeeds against an archived
  repository offline, or the `404` is documented as a deliberate deviation.

## Verification

```sh
cargo test --all-features
nix flake check
```

## Risks and assumptions

An unrecognized route answers `404` with an empty body, indistinguishable from a
missing repository. Changing that is outside this issue.
