---
status: open
priority: P2
related_adrs:
  - ADR-0020
created: 2026-09-24
updated: 2026-09-24
---
# Issue 0079: Make the tree response faithful to the Hub schema

- Status: Open
- Priority: P2
- Related ADR: ADR-0020

## Objective

Report repository contents in the shape the Hub reports them, so a client or tool that
reads the response according to Hub semantics is not misled.

## Problem

`GET /api/{models|datasets}/{ns}/{repo}/tree/{revision}` returns a flat array of
`{type, path, size, oid}` built from the manifest (`src/http.rs:490`), with `oid` set to
the recorded sha256 for every file.

The Hub does not mean that by `oid`. There, a file's `oid` is its git blob hash, and an
LFS-managed file additionally carries `lfs: {oid, size, pointerSize}` where `oid` is the
sha256. The Hub also emits directory entries and paginates.

So ModelKeep's response is Hub-shaped but not Hub-semantics: a tool that compares `oid`
against a git blob hash it computed itself finds a mismatch on every file. The supported
clients tolerate it for downloading — the integration checks pass — but the field does not
mean what its name says.

Since Issue 0078 the value is at least coherent within ModelKeep: `oid` is the same sha256
the resolve route now advertises as `ETag`, which makes the route directly usable for
verifying a local copy. Any change here must keep that property, because it is the only way
a client can check what it holds.

## Dependency

Issue 0074 records upstream per-file metadata from the `repo_info(files_metadata=True)`
call the helper already makes, and that response carries both `blob_id` — the git blob hash
— and `lfs.sha256`. Without that recording, ModelKeep has no git blob hash for any file and
cannot populate `oid` faithfully. **This issue depends on Issue 0074.**

## Scope

- Populate `oid` and `lfs` as the Hub does, from the recorded upstream metadata.
- Keep a verifiable content digest reachable for every file, and keep it equal to what the
  resolve route advertises.
- Decide whether to emit directory entries and whether to paginate, from what the supported
  clients and the documented API actually require — measured, not assumed. A large
  repository's tree is currently returned in one response.
- State in `docs/modelkeep-api.md` which field a client should use to verify a local file.

## Acceptance criteria

- For an LFS-managed file, `oid` is the git blob hash and `lfs.oid` is the sha256, matching
  what the Hub returns for the same file.
- The value a client needs in order to verify bytes it holds is documented, present for
  every file ModelKeep holds, and equal to the `ETag` the resolve route serves.
- A revision with no recorded upstream metadata still answers, and what it reports is
  documented rather than silently different.
- Both pinned clients continue to download and to list correctly, verified by the existing
  integration checks plus a case that reads the tree and verifies bytes against it.

## Verification

```sh
nix develop -c cargo fmt --check
nix develop -c cargo clippy --all-targets --all-features -- -D warnings
nix develop -c cargo test --all-features
nix flake check
```

Compare a fixture's tree response field by field against a recorded observation of the
Hub's response for a repository of each shape — LFS and non-LFS — and keep that observation
under `docs/observations/`.

## Risks and assumptions

Changing `oid` from a sha256 to a git blob hash removes the digest from where a verification
recipe currently reads it. Anything already relying on today's shape breaks unless the
digest remains reachable and documented, which the acceptance criteria require. The
fidelity gained has to be weighed against that: if no supported client or tool actually
reads `oid` as a git hash, the honest alternative is to document the deviation instead of
changing the field.
