# Hugging Face client LFS-reporting observation — 20260924+0900

This is an upstream client observation, not a ModelKeep policy definition. It records
what the two pinned `huggingface_hub` versions do with an `lfs` object in a metadata
answer, and whether an unknown property in the same answer disturbs them, because Issue
0079 has to decide from measurement whether ModelKeep may report `lfs` and may add a
property of its own.

Upstream behavior can change. Re-observe before changing the metadata response shape.

## Environment

- `huggingface_hub` `1.27.0` and `0.36.0`, the two versions pinned by `flake.nix`, built
  from that flake (`pythonForHfClient`) so the versions are the deployed ones
- Two ways of observing, kept apart below: reading the pinned client sources in the Nix
  store, and driving each real client against a stand-in server whose metadata answers
  vary per variant while its `resolve` answers do not
- The stand-in serves ModelKeep's own resolve shape — `ETag` a quoted sha256 of the
  bytes, `x-repo-commit`, `Content-Length`, no `x-linked-etag` — so only what the
  metadata routes say differs between variants
- `is_xet_available()` was **false** in both pinned environments
- Observed while implementing Issue 0079

## What was observed

### An `lfs` object without `pointerSize` fails the download, in both versions

| Variant | `1.27.0` | `0.36.0` |
| --- | --- | --- |
| no `lfs` | downloads; blob named after the served `ETag` | same |
| `lfs: {oid, size, pointerSize}`, `oid` = the served `ETag` | downloads; blob named after the served `ETag` | same (siblings' key is `sha256`) |
| `lfs` with an `oid` that is **not** the served `ETag` | downloads; blob still named after the served `ETag`; the divergent value is not used | same |
| `lfs` without `pointerSize` | `KeyError: 'pointerSize'`, nothing downloaded | `KeyError: 'pointerSize'`, nothing downloaded |

The `KeyError` is unconditional in both versions and comes from the parse, not from a
download decision: `1.27.0` builds every `tree` entry through
`RepoFile.__init__` → `BlobLfsInfo(size=lfs["size"], sha256=lfs["oid"],
pointer_size=lfs["pointerSize"])` (`hf_api.py:827`), and both versions index
`sibling["lfs"]["pointerSize"]` while parsing `repo_info` siblings
(`1.27.0` `hf_api.py:1084`, `0.36.0` `hf_api.py:902`). So an `lfs` object is
all-or-nothing: a server that reports one must state a pointer size.

### With `lfs` present and no `xetHash`, the validator does not move

In every `lfs` variant above, both versions still issued the per-file `HEAD` on the
`resolve` route and named the cache blob after the `ETag` that `HEAD` returned. The
deliberately divergent `lfs.oid` was never used as a validator and never named a blob.

This is narrower than what ADR-0022 decision 7 currently states. The `HEAD` skip in
`1.27.0` is guarded by four conditions together
(`file_download.py:_xet_file_metadata_from_tree_cache`, called from
`_get_metadata_or_catch_error`): `is_xet_available()`, a **valid `xet_hash`**, an
`lfs_sha256` and an `lfs_size`. `lfs.oid` alone does not reach it; with a valid
`xetHash` beside it, the function returns `etag = entry.lfs_sha256` and no `HEAD` is
made. `0.36.0` has no tree-listing cache and no such shortcut at all.

So emitting `lfs` would not move the validator in either pinned version **today**. It
would place a second content digest in a listing, which a client is free to prefer — and
`1.27.0` already prefers it in the Xet case — while requiring a `pointerSize` ModelKeep
has never recorded.

### An unknown property disturbs neither version

With a `modelkeep: {sha256: …}` object on every `tree` entry and every sibling, and an
unknown top-level `modelkeep` key in the `revision` answer, both versions completed
`snapshot_download`, reproduced every file's bytes, and named each blob after the served
`ETag`.

- `1.27.0`: `ModelInfo.__init__` pops the keys it knows and ends with
  `self.__dict__.update(**kwargs)`, so an unknown top-level key is kept rather than
  rejected; `RepoFile.__init__` does the same for a `tree` entry. An unknown sibling key
  is dropped, because siblings are built field by field.
- `0.36.0`: the same for the top-level key; unknown sibling and tree-entry keys are
  ignored. **This was the open half of the Issue 0079 assumption and is now measured**,
  both against the sources and by a real download.

### The siblings' object-id key is `blobId`, not `blob_id`

Both versions read `sibling.get("blobId")` when parsing `repo_info` siblings
(`1.27.0` `hf_api.py:1084`, `0.36.0` `hf_api.py:902`) and expose it as the attribute
`RepoSibling.blob_id`. A sibling that spells the wire key `blob_id` leaves the client's
attribute `None`; measured, by a real client, against a server that spelled it that way.
The `tree` route spells the same value `oid` (`RepoFile.__init__` pops `oid` and exposes
`RepoFile.blob_id`). The attribute names are not the wire names.

## Consequence for ModelKeep

- ModelKeep reports **no `lfs` object and no `xetHash`**, on either metadata route,
  including for a file upstream manages with LFS. ADR-0022 decision 7's conclusion
  stands; its stated reason is narrowed by the measurement above.
- ModelKeep's own digest is reported as a per-file `modelkeep: {sha256}` object on both
  routes, and is the field a caller verifies bytes against
  ([`modelkeep-api.md`](../modelkeep-api.md)).
- The Hub's fields keep the Hub's meanings: `oid` (`tree`) and `blobId` (`revision`
  siblings) are upstream's recorded git object id, or `null` where none was recorded.

## Reproducing

Be precise about what is pinned and what was measured by hand:

- **Pinned.** `hf-client-integration-1-27` and `hf-client-integration-0-36` drive both
  real clients through a mirror whose fixture records a git object id for every file and
  an LFS sha256 for the `.safetensors` files. They require, for every file: `oid` and
  `blobId` equal to the recorded git object id and different from the digest,
  `modelkeep.sha256` equal to the archive's recorded digest and to the `resolve` `ETag`,
  the downloaded bytes verified against `modelkeep.sha256`, no `lfs`, `xetHash` or
  `pointerSize` anywhere in either answer, and `RepoSibling.blob_id` /
  `RepoFile.blob_id` populated with `lfs is None` in both clients. The Rust test
  `an_lfs_managed_file_reports_no_lfs_object_and_one_verifiable_digest` fails if the
  recorded LFS digest ever reaches a response body.
- **Measured by hand for this record, not pinned by any check:** the variant table
  above, including the `KeyError: 'pointerSize'` rows and the divergent-`lfs.oid` row,
  and the `blob_id`-spelled-sibling case. They describe what the clients do with answers
  ModelKeep deliberately never sends, so no check against the implementation can assert
  them. The stand-in server and driver used for them are not part of the repository; the
  variants are described above in enough detail to rebuild it.
- The supported-client checks are run with the exit status taken directly, not through a
  pipe:

```sh
nix build --no-link '.#checks.x86_64-linux.hf-client-integration-1-27' > log 2>&1; echo $?
```
