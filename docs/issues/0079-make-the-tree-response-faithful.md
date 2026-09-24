---
status: done
priority: P2
related_adrs:
  - ADR-0020
created: 2026-09-24
updated: 2026-09-24
---
# Issue 0079: Report Hub fields faithfully, and add ModelKeep's own

- Status: Done
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

## The constraint that decides this issue

Issue 0074 measured something that changes what "faithful" can mean here. **With `lfs`
present in a tree entry, `huggingface_hub` 1.27.0 skips its `HEAD` and takes `lfs.oid` as
the file's validator.** So emitting `lfs` does not merely add information: it moves the
validator off the `ETag` ModelKeep computes and serves, onto a value the client read from a
listing.

Today that substitution happens to be harmless, because `lfs.oid` is the sha256 and
ModelKeep's `ETag` is the same sha256. It is harmless by coincidence of the two values, not
by construction, and issue 0078 exists because exactly that kind of coincidence was relied
on before.

ADR-0022 decision 7 therefore currently forbids emitting `lfs` at all, and the tree route
reports the recorded upstream git object id, else ModelKeep's content digest, else nothing.

This issue has to resolve that, not work around it:

- if `lfs` is emitted, it must be shown that the validator the client ends up using is the
  one ModelKeep would have served, and a test must fail if those two ever diverge;
- if that cannot be shown, ADR-0022 decision 7 stands and this issue becomes documenting the
  deviation plus adding ModelKeep's own property, without `lfs`;
- either way ADR-0022 must be updated or superseded to match what is built, not left
  contradicting the code.

## Dependency

Issue 0074 records upstream per-file metadata from the
`repo_info(files_metadata=True)` call the helper already makes, and that response carries
`blob_id` and `lfs.sha256`. Without it ModelKeep has no git blob hash for any file and
cannot populate `oid` faithfully. **This issue depends on Issue 0074.**

## Acceptance criteria

- For a non-LFS file, `oid` is the git blob hash and no `lfs` object is present.
- For an LFS-managed file, either `oid` is the git blob hash and `lfs.oid` is the sha256 as
  the Hub returns them — with a test that fails if the validator the client uses diverges
  from the one ModelKeep serves — or `lfs` is omitted under ADR-0022 decision 7 and the
  deviation is documented. Which one is chosen must follow from measurement.
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

## Implementation status

Implemented on 2026-09-24. Verified on x86_64-linux with `cargo fmt --check`,
`cargo clippy --all-targets --all-features -- -D warnings`, `cargo test --all-features`,
and `nix flake check`, each with its exit status taken directly.

The Hub's fields carry the Hub's meanings: a file's upstream git object id is reported as
`oid` in a tree entry and `blobId` in a `revision` sibling, and is absent where no record
holds one rather than substituted. ModelKeep's digest moved to `modelkeep: {"sha256": ...}`
on both routes, present for every archived file, `null` for a file the archive does not
hold, and equal to the `ETag` the resolve route serves. `docs/modelkeep-api.md` names it as
the field to verify against.

**`lfs` is not emitted, and the reason is not the one this issue was filed with.** Two
measurements, recorded in
[`hugging-face-lfs-reporting-2026-09-24.md`](../observations/hugging-face-lfs-reporting-2026-09-24.md):
an `lfs` object without `pointerSize` makes both pinned clients fail the whole download with
`KeyError: 'pointerSize'`, and ModelKeep records no pointer size, so a faithful `lfs` would
require inventing one. And `lfs.oid` alone does not move the validator: the `HEAD` skip in
1.27.0 needs xet availability, a valid `xetHash`, an LFS sha256 and an LFS size together, so
with `lfs.oid` and no `xetHash` both versions still issue the `HEAD`, name the blob after the
served `ETag`, and ignore a deliberately divergent `lfs.oid`.

That corrects what ADR-0022 decision 7 originally claimed — that `lfs` being present is
enough to move the validator — which was too broad. The decision stands; its reasoning was
replaced.

The wire name for a sibling's git object id is `blobId`, not `blob_id`. That was found by a
real-client check failing with `blob_id=None`, not by reading the client's source.

Unknown-property tolerance is now measured for both versions rather than one.

Four existing assertions that pinned `oid` as the digest were replaced by six that assert
`oid` is upstream's object id, assert it is **not** the digest, and assert the digest in its
new home — so the two fields cannot quietly merge again.

**Remaining**: directory entries and pagination are untouched; a large repository's tree is
still returned in one response. Emitting a faithful `lfs` later requires recording a pointer
size, which is an addition to ADR-0022 decision 1 and a change to the helper.
