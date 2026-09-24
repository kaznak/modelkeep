# Hugging Face client validator observation — 2026-09-24 JST

This is an upstream client observation, not a ModelKeep policy definition. It records
how the supported `huggingface_hub` versions use the validator a server advertises for
a file, because that value is not only an HTTP cache token to them: it is the **name of
the blob** in the client's content-addressed cache, so a server that advertises a value
two different files can share makes the client store one file's bytes under both paths.

Upstream behavior can change. Re-observe before changing what ModelKeep advertises.

## Environment

- `huggingface_hub` `0.36.0` and `1.27.0`, the two versions pinned by `flake.nix`
- A local ModelKeep over a local fixture revision; no public repository was downloaded
- Header shapes were varied by a rewriting proxy in front of a real ModelKeep, so the
  server behavior stayed real and only the advertised validator changed
- The fixture revision is `tests/fixtures/hf_fetch_collision_fixture.py`: four equally
  sized shards with different bytes, plus two byte-identical files
- Observed while implementing Issue 0078

## What was observed

### The cache is keyed by the validator, in both versions

A cold `snapshot_download` into a `cache_dir` writes
`blobs/<validator>` and links `snapshots/<commit>/<path>` to it. The blob file name was
the advertised validator verbatim in every variant below, in both versions.

The consequence is direct: two paths advertised with one validator become two links to
one blob, and the file fetched second overwrites the first. With the pre-fix
`"{commit}-{size}"` validator and four equally sized shards, both versions reported
success and left **three of the four shards holding another shard's bytes** — file count
and byte total correct, `ls -lL` and `find -L -type l` clean.

### Which validator shapes each version accepts

| Advertised | `0.36.0` | `1.27.0` |
| --- | --- | --- |
| `ETag: "{commit}-{size}"` (pre-fix) | downloads, 4 blobs for 8 files, 3 shards corrupt | downloads, 4 blobs for 8 files, 3 shards corrupt |
| `ETag: "<sha256 hex>"` | downloads, blob per digest, every file's bytes correct | same |
| `ETag: <sha256 hex>` unquoted | accepted, same result | accepted, same result |
| `ETag` unchanged + `x-linked-etag: "<sha256 hex>"` | accepted, blob named by the linked value | same |
| `ETag: "<sha256 hex>"` + `x-linked-etag: "<sha256 hex>"` | accepted, same result | accepted, same result |

So the digest in `ETag` alone is sufficient for both versions, and `x-linked-etag` is
not required to make the client use a content-addressed name. ModelKeep therefore does
not send `x-linked-etag`, which keeps the existing guarantee that its responses carry no
Xet-linked headers.

**A sha256 is accepted for every file, not only for LFS-like ones.** The real Hub
returns a file's sha256 for an LFS-managed file and a git blob hash for a small non-LFS
file, and ModelKeep records a sha256 for every file without recording what upstream
advertised. The fixture's small JSON files (`config.json`,
`model.safetensors.index.json`, the duplicate pair) were served with their sha256 and
both versions accepted them, named their blobs after them, and reproduced their bytes.
Nothing in either client was observed to require a particular validator *shape* per file
class, so recording the upstream validator during acquisition was not needed.

### Byte-identical files share one blob, which is correct

With the digest advertised, the two byte-identical files carried one validator, and both
versions stored **one blob with two links to it**: 7 distinct blobs for 8 files. Both
files' bytes were correct. This is the same deduplication the Hub gets from an LFS
`oid`, and it is the reason the fix must make the validator *be* the content
fingerprint rather than merely make it unique per path.

### What a warm client does when the validator changes

This matters operationally, because it decides whether the server-side fix repairs
anything on its own. Two cases, both measured in both versions:

- **A complete snapshot for the same commit is already materialized.** Neither version
  revalidates: no `HEAD`, no `GET`, no request on the `resolve` route at all. The client
  returns its existing files. A cache corrupted by the old validator therefore **stays
  corrupt and stays silent**; the fix cannot repair it. The same holds for a `local_dir`
  download whose `.cache/huggingface` metadata records that commit.
- **The file has to be fetched again** (fresh cache, or the snapshot pointer for that
  commit is absent while the blobs remain). Both versions re-download the bytes,
  because the new validator names a blob the cache does not have — the old,
  colliding blobs are left in place and not reused. Every file's bytes then matched the
  archive's recorded digests.

## Consequence for ModelKeep

- The advertised validator is the file's recorded sha256, quoted, in `ETag`, on the
  whole response, on `HEAD`, and on a `206`. No `x-linked-etag`.
- `If-None-Match` is compared against that same value, so a `304` means the client holds
  these bytes.
- Deduplication of byte-identical files is a correct outcome to preserve, not a
  collision to remove.
- Clients that downloaded a sharded repository from a deployment older than this change
  must clear their caches; nothing the server does will refresh them.

## Reproducing

Be precise about what is pinned and what was measured by hand:

- **Pinned.** `hf-client-integration-0-36` and `hf-client-integration-1-27` drive a real
  client through the same-size/byte-identical fixture and require, for every file, that
  the downloaded bytes match the digest the archive recorded — compared as a digest, not
  as a length — that the blob the client chose is named after that digest, that the four
  equally sized shards occupy four distinct blobs, and that the two identical files share
  one. They also pin `ETag` on `HEAD`, on a `206`, the absence of `x-linked-etag`, and
  that another file's validator does not produce `304`. The Rust tests in `src/http.rs`
  pin the same contract against the server alone.
- **Measured by hand for this record, not pinned by any check:** the rows of the table
  above other than the two the implementation depends on (pre-fix collision and digest
  in `ETag`), the acceptance of an unquoted digest, the `x-linked-etag` variants, and
  both warm-client cases. The pre-fix corruption was reproduced by mutating a copy of the
  working tree, which no check can assert against the fixed implementation.

Treat every unpinned row as an observation that can go stale, and re-measure before
relying on it. The supported-client checks are run with the exit status taken directly,
not through a pipe.

## Checked against the Hub on the reported repository

The field report that prompted Issue 0078 named `Qwen/Qwen3.8-27B` at
`1d4bf0f2ff6012fd82039f2fa52739d0dd7c60c0`, and quoted two shards of identical length that
ModelKeep had given identical ETags. That revision is archived on the deployment, so the fix
was checked against the exact object.

Both shards are still `3979553696` bytes, and the validators are now different. Then every
archived `.safetensors` file of that revision was compared with the Hub's own per-file digest,
read from `repo_info(files_metadata=True)`:

```text
18 archived safetensors files checked, 0 digest mismatches
```

Two things follow, and the second was not expected.

The archive holds byte-correct data for all eighteen shards. The corruption in the report was
confined to the client's cache, as the report itself judged, and nothing needs re-fetching.

**ModelKeep's validator equals the Hub's `lfs.sha256` for every one of these files.** Issue
0078 chose to serve the digest ModelKeep records rather than to record and relay upstream's
validator, on the grounds that a content fingerprint is what the clients need. For
LFS-managed files the two turn out to be the same value, so the served validator is also
faithful to what the Hub would return, without ModelKeep recording anything extra. That is a
property of LFS objects being addressed by their sha256, not a coincidence to rely on for
non-LFS files, where the Hub's validator is a git blob hash and ModelKeep's is not.

### A correction to the report

The report quoted upstream `x-linked-etag` values for the two shards:

```text
model-00006-of-00018.safetensors  0bc5214fac607f0e6cc92eec3789d4b8559410ef9fce66621ba8158e8410dae0
model-00008-of-00018.safetensors  80b0c49033e9a0d5762562aa12f4acdb7f54da586f3d0110f28c48d91cf07892
```

The first matches both the Hub and ModelKeep. The second matches neither: the Hub reports a
different digest for `model-00008-of-00018.safetensors`, and ModelKeep serves that same
different digest. The `80b0c490…` value belongs to some other file or request. The report was
assembled from two hand-issued `curl` calls, so a pairing slip is the likely explanation.

This is recorded because the alternative reading — that the archive holds wrong bytes for that
shard — would have been serious, and it was ruled out by measurement rather than by argument.
