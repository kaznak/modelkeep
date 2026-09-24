# Hugging Face client tree-listing observation — 20260924+0900

This is an upstream client observation, not a ModelKeep policy definition. It records
what the two pinned `huggingface_hub` versions take from the repository metadata routes
and what they do with each field, because ModelKeep now answers those routes for a
revision it has never archived and has to know which fields are load-bearing.

Upstream behavior can change. Re-observe before changing the metadata response shape.

## Environment

- `huggingface_hub` `1.27.0` and `0.36.0`, the two versions pinned by `flake.nix`
- Read from the pinned `1.27.0` sources in the Nix store, and from the behavior the
  supported-client checks exercise for both versions
- Observed while implementing Issue 0074

## What was observed

### Which route supplies the file list

`0.36.0` takes its file list from `repo_info`, that is from `siblings[].rfilename` on
`/api/models/{repo}/revision/{revision}`.

`1.27.0` takes its file list from `list_repo_tree`, that is from
`/api/models/{repo}/tree/{revision}`, and calls `repo_info` only to resolve a revision
that is not already a commit id. For a commit-pinned download it skips `repo_info`
entirely and the `tree` route is the **only** metadata request it makes.

So both routes have to answer for a revision the archive has never seen, and they have
to answer with the same file set, or the two clients disagree about what a repository
contains.

### Which `tree` fields are required

`1.27.0` parses each entry into `RepoFile`, whose constructor reads `path`, `size` and
`oid` unconditionally: a missing key raises `KeyError` and fails the download. A JSON
`null` is accepted for `size` and `oid` — both are carried into the client's on-disk
tree listing cache and are not used arithmetically or compared against anything.

A `tree` entry may also carry `lfs` (`{oid, size, pointerSize}`) and `xetHash`. These
are not inert: when an entry carries **both** a valid `xetHash` and `lfs.oid`/`lfs.size`,
and Xet support is installed, `1.27.0` rebuilds the file metadata from its cached tree
listing and **skips the per-file `HEAD` request**, taking `lfs.oid` as the `ETag` and
`lfs.size` as the file length.

That is why ModelKeep reports neither field. Advertising them would replace the
validator ModelKeep verified and serves with a value it merely relayed, and would remove
the `HEAD` request through which ModelKeep answers for the bytes it is about to serve.
Reporting `lfs` without `xetHash` does not trigger the shortcut today, but it would
still be an unverified content digest presented as this file's identity.

### The client's tree listing cache

`1.27.0` writes the listing it received to `<local_dir>/.cache/huggingface/trees/
<commit>.json` (or under the cache directory for a cache download) and reuses it on a
later run, which skips the `tree` request. It also uses it to decide whether an existing
snapshot is complete for the requested patterns. A listing that understated a
repository therefore persists on the client after ModelKeep stops understating it.

## Consequence for ModelKeep

- both metadata routes answer from one file list, so `siblings` and `tree` cannot drift;
- every `tree` entry carries `path`, `size` and `oid` keys, with `null` where ModelKeep
  has no value to state rather than an invented one;
- `oid` is upstream's recorded git object id where the revision has one, and otherwise
  ModelKeep's own content digest, which is the `ETag` value. The precedence is
  documented in [`modelkeep-api.md`](../modelkeep-api.md);
- `lfs` and `xetHash` are never reported.

## Reproducing

The field requirements were read from the pinned client sources
(`huggingface_hub/hf_api.py`, `_snapshot_download.py`, `_tree_cache.py`,
`file_download.py`). The behavior that depends on them is pinned by the
`hf-client-integration-1-27` and `hf-client-integration-0-36` checks, which drive both
real clients through a filtered download of a repository the mirror has never seen and
require the archived set to be the filtered set. Run them with the exit status taken
directly rather than through a pipe:

```sh
nix build --no-link '.#checks.x86_64-linux.hf-client-integration-1-27' > log 2>&1; echo $?
```
