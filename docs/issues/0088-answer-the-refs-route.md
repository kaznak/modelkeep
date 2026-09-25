---
status: done
priority: P1
related_adrs:
  - ADR-0012
  - ADR-0022
created: 2026-09-26
updated: 2026-09-26
---
# Issue 0088: Answer the refs route, which llama.cpp needs before any file

- Status: Done
- Priority: P1
- Related ADR: ADR-0012, ADR-0022

## Objective

`GET /api/{models,datasets}/{namespace}/{repo}/refs` answers the Hub's shape with the
commit `main` resolves to, so llama.cpp's `--hf-repo` can download through ModelKeep.

## Problem

Reported from the GX10 deployment on 2026-09-26: llama-server (llama.cpp v0.4.1) with
`HF_ENDPOINT` set to ModelKeep resolves the commit it downloads from
`branches[name == "main"].targetCommit` on the `refs` route, before it asks for any
file list. ModelKeep had no such route and answered `404` with an empty body, so
llama.cpp resolved no commit, listed no files, and never started the download. The Hub
answers the same request `200` with `{"tags":[],"branches":[{"name":"main","ref":
"refs/heads/main","targetCommit":"<commit>"}],"converts":[]}`.

The llama.cpp request sequence (`refs`, then `tree/{commit}?recursive=true`, then
`HEAD` and a ranged `GET` on `resolve/{commit}/{path}`) is taken from the report's
reading of the llama.cpp sources. It was not re-observed here: neither llama.cpp's
sources nor the Hub were reachable from the implementing environment.

## Scope

- Answer `refs` for models and datasets with `main` only, resolved exactly as
  `revision/main` is: archived ref first, never contacting upstream when it exists;
  otherwise upstream's commit, acquiring nothing.
- `include_prs=1` adds an empty `pullRequests`, which both pinned clients index.
- Other branches and tags are not listed; the archive does not record which a ref is.

## Acceptance criteria

- Both pinned `huggingface_hub` versions parse `list_repo_refs` against ModelKeep, cold
  and offline, for models and datasets, and see `main` at the archived commit.
- An archived `main` answers without an upstream call.
- An unarchived repository answers upstream's commit and publishes nothing.
- Upstream failures classify as on the metadata routes (`502`, `404`, `401`).
- The llama.cpp request sequence completes against an offline mirror.

## Verification

```sh
cargo test --all-features refs_route
nix build --no-link '.#checks.x86_64-linux.hf-client-integration-1-27'
nix build --no-link '.#checks.x86_64-linux.hf-client-integration-0-36'
```

## Risks and assumptions

A repository whose default branch is not `main` reports `main` as absent upstream
(`404`), as `revision/main` already does.
