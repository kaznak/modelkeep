---
status: open
priority: P2
related_adrs:
  - ADR-0020
created: 2026-09-24
updated: 2026-09-24
---
# Issue 0079: Report Hub fields faithfully, and add ModelKeep's own

- Status: Open
- Priority: P2
- Related ADR: ADR-0020

## Objective

Report repository contents in the shape the Hub reports them, so a tool reading the response
by Hub semantics is not misled, and carry ModelKeep's verification digest as an additional
property rather than by redefining one of the Hub's.

## Problem

Two metadata routes report contents, and neither matches the Hub.

`GET /api/{models|datasets}/{ns}/{repo}/tree/{revision}` returns a flat array of
`{type, path, size, oid}` from the manifest (`src/http.rs:490`), with `oid` set to the
recorded sha256 for every file. The Hub means something else by `oid`: there it is the
file's git blob hash, and an LFS-managed file additionally carries
`lfs: {oid, size, pointerSize}` where `oid` is the sha256. The Hub also emits directory
entries and paginates. So a tool comparing our `oid` against a git blob hash it computed
finds every file mismatched.

`GET /api/{models|datasets}/{ns}/{repo}/revision/{revision}` is thinner still: its
`siblings` carry `rfilename` and nothing else (`src/http.rs:391`), where the Hub's
`files_metadata` form carries `size`, `blob_id` and `lfs` per sibling.

Neither is breakage — the supported clients download correctly and the integration checks
pass — but the fields do not mean what their names say, and the digest a client needs in
order to verify what it holds is reachable only by reading a field that means something
else.

## Approach

Keep the Hub's field names with the Hub's meanings, and add ModelKeep's own information as
an additional property rather than by overloading theirs.

- `oid` becomes the git blob hash, and an LFS-managed file carries `lfs: {oid, size,
  pointerSize}`, as the Hub does.
- A namespaced ModelKeep property carries the digest ModelKeep serves as the file's `ETag`,
  present for **every** file the archive holds, whether or not the file is LFS-managed. A
  nested object rather than a bare key, so later additions do not each need a new name.
- The `revision` route's siblings gain the Hub's per-file fields and the same ModelKeep
  property.

This is what makes the two goals independent. Fidelity stops being a trade against
verifiability: a Hub-semantics tool reads the Hub fields, and a client verifying its own
copy reads one ModelKeep field that is always there and always equals the `ETag` it was
served.

**An unknown property must be shown not to disturb the supported clients.** Partly
established already, on 2026-09-24 against `huggingface_hub` 1.27.0: `ModelInfo.__init__`
pops the keys it knows and ends with `self.__dict__.update(**kwargs)`, so an unknown
top-level key is kept rather than rejected, and constructing a sibling carrying an unknown
key yielded `RepoSibling(rfilename=..., size=10, blob_id=None, lfs=None)` without error, so
an unknown per-file key is ignored. **0.36.0 is not yet checked** — it is not in the
development shell — and must be before this is treated as settled.

## Dependency

Issue 0074 records upstream per-file metadata from the
`repo_info(files_metadata=True)` call the helper already makes, and that response carries
`blob_id` and `lfs.sha256`. Without it ModelKeep has no git blob hash for any file and
cannot populate `oid` faithfully. **This issue depends on Issue 0074.**

## Acceptance criteria

- For an LFS-managed file, `oid` is the git blob hash and `lfs.oid` is the sha256, matching
  what the Hub returns for the same file. For a non-LFS file, `oid` is the git blob hash and
  no `lfs` object is present.
- Every file the archive holds carries the ModelKeep property with a digest equal to the
  `ETag` the resolve route serves for it, and `docs/modelkeep-api.md` names that property as
  the one to verify against.
- Both pinned clients download and list correctly with the added property present, and a
  check reads the tree and verifies bytes against the ModelKeep property.
- A revision with no recorded upstream metadata still answers. What it omits — a git blob
  hash it never recorded — is documented rather than filled with a substitute.
- Whether directory entries and pagination are added is decided from what the clients and
  the documented API require, measured rather than assumed. A large repository's tree is
  currently returned in a single response.

## Verification

```sh
nix develop -c cargo fmt --check
nix develop -c cargo clippy --all-targets --all-features -- -D warnings
nix develop -c cargo test --all-features
nix flake check
```

Compare a fixture's responses field by field against a recorded observation of the Hub's
responses for a repository of each shape, LFS and non-LFS, and keep that observation under
`docs/observations/`.

## Risks and assumptions

The digest moves out of `oid`. The only thing known to read it there is a verification
recipe written on 2026-09-24, so the cost is documenting the new location — which the
acceptance criteria require anyway. Tolerance of an unknown property is established for 1.27.0 and
still open for 0.36.0; if 0.36.0 rejects it, the fallback is to keep the Hub fields faithful
and publish the digest on a separate route rather than to overload `oid` again.
