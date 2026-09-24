# ModelKeep client and API guide

ModelKeep exposes two separate interfaces with different trust and operational
boundaries:

| Interface | Purpose | Preferred client |
|---|---|---|
| Download endpoint | Read model or dataset metadata and files; archive a missing snapshot through pull-through | supported `hf` or `huggingface_hub` client |
| Admin endpoint | Inspect inventory and run explicit prefetch, refresh, verify, or audit jobs | versioned Admin API |

The download endpoint is a supported subset of the Hugging Face Hub protocol, not a
general-purpose management API. The Admin endpoint must never be used as
`HF_ENDPOINT`. See [`admin-api.md`](admin-api.md) for its routes and authorization
rules.

Do not put a deployment hostname, token, or operator identity in tracked commands or
agent instructions. Obtain both origins from an ignored site configuration or
explicitly supplied environment variables:

```sh
MODELKEEP_ENDPOINT=${MODELKEEP_ENDPOINT:-$(
  jq -er '.endpoint' qnap-acceptance.config.json
)}
MODELKEEP_ADMIN_ENDPOINT=${MODELKEEP_ADMIN_ENDPOINT:-$(
  jq -er '.admin_endpoint' qnap-acceptance.config.json
)}
export MODELKEEP_ENDPOINT MODELKEEP_ADMIN_ENDPOINT
```

## Download models and datasets

Use a supported official client rather than constructing compatibility requests by
hand. This preserves the same behavior covered by ModelKeep's client integration
tests.

```sh
HF_ENDPOINT="$MODELKEEP_ENDPOINT" hf download org/model --revision <revision>
HF_ENDPOINT="$MODELKEEP_ENDPOINT" hf download org/dataset \
  --repo-type dataset --revision <revision>
```

For Python, either set `HF_ENDPOINT` before starting the process or pass the endpoint
explicitly:

```python
import os
from huggingface_hub import snapshot_download

snapshot_download(
    repo_id="org/model",
    revision="<revision>",
    endpoint=os.environ["MODELKEEP_ENDPOINT"],
)
```

Use `repo_type="dataset"` for a dataset. Prefer an immutable 40-character commit SHA
when the caller needs a reproducible snapshot. A mutable ref such as `main` is
resolved and recorded separately; advancing it does not replace an already published
immutable revision.

A request for an archived snapshot is a warm read. A request for a missing ref or file
starts upstream acquisition and publishes to durable storage. Consequently, an
apparently read-only client command can consume substantial network and archive
capacity. Before requesting an unknown large repository, establish its likely size and
obtain authorization appropriate to that cost. A request for a single file the archive
does not hold acquires that file; repository metadata acquires nothing.

### A client's own filter narrows the first acquisition

`--include` / `allow_patterns` works on a repository the mirror has never seen. The
client reads repository metadata before it requests any file, and ModelKeep answers
that from upstream's file list **without acquiring anything**, so the per-file requests
that follow transfer only what the filter kept:

```sh
HF_ENDPOINT="$MODELKEEP_ENDPOINT" hf download org/model --include 'q4/*'
```

Two consequences follow, and both matter operationally:

- a metadata request costs one upstream metadata round trip and writes nothing to the
  archive. Nothing is published, cached, or reserved by reading metadata;
- **a download with no filter asks for every file**, so the whole repository is still
  acquired one file at a time. The filter is the only thing that bounds the cost, and a
  forgotten one is stopped by cancelling the running acquisition through the
  [Admin API](admin-api.md).

A filtered `prefetch` through the [Admin API](admin-api.md) remains the way to archive
a subset ahead of time, and is still preferable for a large repository, because it is
asynchronous and reports progress.

### A revision may hold a subset of its repository

The archive records what it holds and asserts nothing about upstream completeness
([`ADR-0020`](adr/0020-selection-scoped-revision-acquisition.md)). A revision may
therefore hold a subset of the upstream repository — from a filtered prefetch through
the [Admin API](admin-api.md), from a single-file request, or from an imported cache. A
request for a path a revision does not hold is a miss, not a `404` from the archive:
whether the path exists is upstream's answer, and when upstream has it the file is
added to that same revision rather than published as a second one. A published path is
never overwritten or removed.

What repository metadata reports depends on whether the revision knows its commit's
upstream file list. ModelKeep records that list when an acquisition obtains it, which
it does from the same upstream call it already makes to resolve the revision. A commit
is immutable, so its file list cannot go stale:

| Revision state | Repository metadata reports |
|---|---|
| Archived, with a recorded file list | every file the commit holds upstream, including the ones the archive does not |
| Archived, without one — published before this existed, or imported from a client cache | exactly the archived set, as before |
| Not archived | every file upstream reports, obtained without acquiring |

Reporting a file the archive does not hold is deliberate. A partially archived revision
that reported only its subset handed a client a sharded model with shards missing and
let it report a successful download. A client that asks for a reported file gets it,
because the request extends the same revision from upstream — so an unfiltered download
of a partially archived revision **fails while upstream is unavailable** instead of
silently completing without those files. That failure is the correct outcome; use a
filter that matches what the archive holds, or prefetch the rest.

ModelKeep never fabricates a metadata entry. A revision whose file list is unknown
reports what it holds, and a metadata answer that could not be obtained is a
classified failure (`502` when upstream is unreachable), never an empty or partial file
list presented as complete.

Cancelling an acquisition releases the requests waiting on it with `502`, and a request that
arrives afterwards starts new work. That is deliberate, and it has a consequence worth
knowing before relying on cancellation: measured against both supported client versions, a
client does not stop at that `502` — it retries within seconds and the transfer restarts.
Cancelling stops the transfer in flight, not the client driving it. Stop the client too, and
use the in-flight acquisition view to confirm the work has not come back.

ModelKeep serves payloads itself and does not redirect a client to Hugging Face or
Xet. Do not add fallback logic that silently changes `HF_ENDPOINT` or follows a
payload path around ModelKeep. A warm archived revision is expected to remain
downloadable while upstream access is unavailable. That guarantee covers the files the
archive holds; a request for a path it does not hold needs upstream, and fails with an
upstream error (`502` when upstream is unreachable) rather than silently reporting the
path as absent.

## Cold-miss latency contract

A file request the archive cannot serve needs upstream. That acquisition can take
minutes or hours for a large file, so a `resolve` request does not wait for it
indefinitely.

The two kinds of route are bounded differently, because the supported clients treat
them differently. Both behaviors below were measured against `huggingface_hub`
0.36.0 and 1.27.0, not assumed.

### `resolve` is bounded by default

- `GET` and `HEAD` on `/{namespace}/{repo}/resolve/{revision}/{path}` wait for the
  acquisition for at most the cold-miss deadline, **8 seconds** by default. The
  deadline applies to both methods identically.
- If the file is archived inside that window, it is served normally.
- Otherwise ModelKeep answers `503 Service Unavailable` with a `Retry-After` header
  holding the deadline in seconds. The acquisition is **not** cancelled: it keeps
  running, and a retry joins it instead of starting a second download. A retry after
  a `503` therefore costs nothing upstream.
- `MODELKEEP_COLD_MISS_DEADLINE_SECONDS` configures this deadline in whole seconds.
  `0` restores an unbounded wait, which holds the connection open with no response
  headers for the whole transfer and is not recommended.

The default is set by the supported clients' own patience: both abandon a `resolve`
request after 10 seconds with a read timeout, which is the `status=000` that
motivated this contract. Answering at 8 seconds keeps ModelKeep's status inside that
window. Both versions then retry a `503` on their own — 0.36.0 with its own
1s/2s/4s/8s/8s backoff, 1.27.0 following `Retry-After` — so a cold miss that
completes within roughly a minute is transparently recovered by the client.

### Repository metadata does not wait for an acquisition

`GET` on `/api/.../revision/{revision}` and `/api/.../tree/{revision}` for a revision
the archive has never seen is answered from upstream's file list, which costs one
upstream metadata round trip and no transfer. There is nothing to wait for, so the
deadline below does not normally apply at all.

It still applies on one path: a fetch helper that cannot report upstream's per-file
metadata leaves ModelKeep with no file list to answer from, and the request falls back
to acquiring the revision and answering from the archive. That fallback waits for the
whole repository acquisition to finish. It is **not** bounded by the `resolve`
deadline, and by default it is not bounded at all, so the request can stay open for
minutes or hours on a large repository.

That default is deliberate, and it rests on two measurements:

- Neither client applies its 10-second `resolve` read timeout to a metadata request.
  0.36.0 waited 30 s and 1.27.0 waited 12 s per metadata request, and both then
  completed the download. A metadata cold miss is slow, not broken.
- Neither client retries a bounded metadata answer. `503`, and equally `429`, `425`,
  `500` and `504`, each end the call on the first response: `snapshot_download`
  raises `LocalEntryNotFoundError` and `list_repo_tree` raises `HfHubHTTPError`.
  There is no status that buys a client-side retry here, unlike on `resolve`.

Bounding metadata by default would therefore make any request that falls back to an
acquisition fail outright rather than slowly succeed. 1.27.0 requests `revision` and
then `tree` during a download, and takes its file list from `tree`; 0.36.0 requests
`revision` only and takes its file list from `siblings`. Both routes therefore have to
answer for a revision the archive has never seen, and they answer with the same file
set.

An operator who prefers a prompt answer over a long wait can opt in:

- `MODELKEEP_METADATA_COLD_MISS_DEADLINE_SECONDS` bounds the metadata routes in whole
  seconds. It is **`0` by default, meaning wait for the acquisition**. It is a
  separate setting from `MODELKEEP_COLD_MISS_DEADLINE_SECONDS` and neither changes
  the other.
- When it is set and the deadline passes, the metadata routes answer exactly as
  `resolve` does: `503` with `Retry-After`, the acquisition still running, and a
  repeated request joining it rather than starting a second download.
- **The cost is a failed download.** Because neither supported client retries, every
  cold `hf download` whose acquisition outlasts the configured deadline fails with
  `LocalEntryNotFoundError` instead of completing. Repeating the command joins the
  running acquisition and succeeds once the revision is archived, but the client will
  not do that by itself. Set this only where a prompt, classified answer is worth
  that trade — for example behind a proxy that would drop the connection anyway.

`503` from either kind of route always means "acquiring, retry"; it never means the
repository, revision, or file is absent, and no partial data is published or served
because a request was cut off. A metadata `503` is never replaced by a `200` holding
an empty or partial file list. Operationally, an `archive_miss` event is followed
either by `acquisition_progress` events carrying a byte count that actually
advanced, or — where a deadline applies — by `acquisition_deadline_exceeded`; a byte
counter that is not moving is never reported as progress.

An already archived revision never enters this path: its metadata and files are
answered from the archive and are unaffected by the deadline.

### A cancelled acquisition answers `502`

An operator can stop an acquisition that is already running, through the
[Admin API](admin-api.md). A request that was waiting for it is answered
`502 Bad Gateway` — the existing "acquisition failed" class — because the acquisition it
was waiting for did not complete.

It is deliberately **not** `503`. `503` means "acquiring, retry", and both supported
clients retry it by themselves; answering `503` would restart the transfer the operator
just stopped. It is also never `404`: nothing was learned about whether upstream holds
the file, and a cancellation must not be reported as an absence. And it is never a
success, because nothing was published — no partial file is ever served.

**A new request after a cancellation starts a new acquisition. That is correct
behaviour, not a defect.** Cancellation is an interruption of one transfer, not a
cooldown, a block, or a policy about the repository; ModelKeep has no state that says
"do not acquire this". The new acquisition also adopts the resumable staging the
cancelled one left, so it starts from the bytes already transferred rather than from
zero. An operator who wants a repository to stay unacquired must stop the requests, not
the acquisition.

Because identical work is collapsed into one acquisition, stopping it answers every
request waiting on it, and a management job sharing it reaches its terminal `cancelled`
state. That follows from sharing one transfer rather than paying for it repeatedly.

### Transfers are serialized per repository

At most one acquisition transfers per repository at a time, and at most a configured
number across all repositories (two by default), under
[`ADR-0021`](adr/0021-one-transferring-acquisition-per-repository.md). A cold request
for a repository that is already being acquired under a different selection therefore
waits for that transfer before its own begins, and the `resolve` deadline above still
bounds how long it waits before answering `503`. Nothing about the archived path changes:
an already archived file is served immediately regardless of what is transferring.

## File validators are content digests

A file response on the `resolve` routes carries an `ETag` that is the sha256 of the
file's bytes, quoted, and nothing else:

```http
ETag: "73ce509c5365f1acd906cce8d6e9339aa2b7b055507cf84fcde50bc97c8a2798"
```

The whole response, a `HEAD`, and a `206` for a byte range all carry the digest of the
**whole file**, and `If-None-Match` is compared against that same value, so a `304`
means the caller holds these bytes rather than merely a file of this length in this
revision. No `x-linked-etag` is sent; the supported clients accept the digest in `ETag`
alone. The measurement behind this shape, for both pinned client versions, is
[`docs/observations/hugging-face-content-validator-2026-09-24.md`](observations/hugging-face-content-validator-2026-09-24.md).

### Verify a file against `modelkeep.sha256`

Both metadata routes carry a per-file `modelkeep` object, and **`modelkeep.sha256` is the
field to verify bytes against**. It is the digest ModelKeep computed over the bytes it
holds, the value it validates its own archive against, and exactly the `ETag` the
`resolve` route serves for that path, without the quotes. It is present for every file
the archive holds, whether or not upstream manages that file with LFS, and it is `null`
for a path ModelKeep does not hold, where there are no bytes it can stand behind.

```jsonc
// GET /api/models/org/model/tree/<commit>
[
  {
    "type": "file",
    "path": "model.safetensors",
    "size": 29,
    "oid": "8095a62ccb4d806da7666fcda07467e2d150218e",
    "modelkeep": { "sha256": "73ce509c…c8a2798" }
  }
]
```

```jsonc
// GET /api/models/org/model/revision/<commit>
{
  "sha": "…",
  "siblings": [
    {
      "rfilename": "model.safetensors",
      "size": 29,
      "blobId": "8095a62ccb4d806da7666fcda07467e2d150218e",
      "modelkeep": { "sha256": "73ce509c…c8a2798" }
    }
  ]
}
```

The object is nested so later additions need no new top-level name; today it holds
`sha256` alone. Both pinned `huggingface_hub` versions ignore it: an unknown top-level
key and an unknown per-file key disturb neither of them, measured for both in
[`docs/observations/hugging-face-lfs-reporting-2026-09-24.md`](observations/hugging-face-lfs-reporting-2026-09-24.md).

### The Hub's fields keep the Hub's meanings

`oid` on the `tree` route and `blobId` on the `revision` route are one value under the
two names the Hub uses for it: **the git object id upstream recorded for that path**. It
is a fact about the commit, recorded verbatim, and is **not** a digest ModelKeep
verified — for a non-LFS file it is a different function of the bytes altogether, and for
an LFS-managed file it is the id of the pointer blob. It is **`null`** when ModelKeep has
no recorded value, and nothing stands in for it: a revision archived before the upstream
file list was recorded, or imported from a client cache, reports `null` there for every
file and is not migrated. ModelKeep's digest is never reported under these names. A
deployment before this change reported it as the `tree` route's `oid` wherever no
upstream object id had been recorded, so a caller that read `oid` as a content digest
must read `modelkeep.sha256` instead.

`size` follows the same principle: it is what ModelKeep will serve when it holds the
file, upstream's recorded length when it does not, and `null` when neither states one.

**No `lfs` object and no `xetHash` are reported**, which is a deliberate deviation from
the Hub for two measured reasons
([`docs/observations/hugging-face-lfs-reporting-2026-09-24.md`](observations/hugging-face-lfs-reporting-2026-09-24.md)):

- the Hub's `lfs` object states a `pointerSize`, and both pinned clients raise
  `KeyError: 'pointerSize'` and fail the whole download when it is missing. ModelKeep
  does not record a pointer size for any file, so it would have to invent one — the same
  substitution this surface exists to remove;
- `lfs.oid` is a content digest, and a client may take it as the file's validator
  instead of requesting the `HEAD` that carries ModelKeep's. Relaying upstream's value
  there would move the validator onto a number ModelKeep never verified.

A caller that wants the content digest therefore reads `modelkeep.sha256`, which is
always ModelKeep's own and always equals what `resolve` serves.

Two consequences follow from a content-derived validator, and both are intended:

- **Files with identical bytes carry the same validator and are allowed to share it.**
  `huggingface_hub` names each blob in its cache after the validator, so it stores one
  blob and links both paths to it. That is correct — it is how the Hub deduplicates LFS
  objects by `oid` — and it is not a collision to be removed.
- **Files with different bytes can never share a validator**, whatever their lengths.
  This is the point of the change: a validator that mixed the commit with the file's
  length was shared by every file of one length in a revision, which is the normal case
  for sharded weights, and a client then stored one shard's bytes under several shards'
  paths while reporting success.

### Upgrading from a deployment that advertised `"{commit}-{size}"`

Releases up to and including v0.4.8 advertised a validator derived from the commit and
the file length. Two things follow for clients, both measured against both supported
versions:

- A client that needs a file again re-downloads it even though it believes it already
  has it, because the blob it holds is named after the old validator. **This is the
  correct outcome**: what it holds may be another file's bytes. It costs one transfer
  per file from ModelKeep's archive, not from upstream.
- A client that already holds a **complete** snapshot of that commit does not
  revalidate at all — no `HEAD`, no `GET` — so a cache that is already corrupt stays
  corrupt and stays silent. **A server-side fix does not repair client caches.** Any
  cache that downloaded a sharded repository from an older deployment has to be deleted
  and re-downloaded. Compare the number of symlinks under `snapshots/` with the number
  of distinct files under `blobs/` — or run `du` on `blobs/` — to detect it; `ls -lL`
  and `find -L -type l` both pass on a corrupt tree, because they resolve the shared
  blob repeatedly.

The archive itself is unaffected: ModelKeep reads the digest each revision's manifest
already records, so no revision is re-acquired.

## Health and compatibility routes

The download origin provides unauthenticated service probes:

```http
GET /healthz
GET /readyz
```

`healthz` shows that the process is alive. `readyz` indicates whether it can serve
requests; check both when diagnosing connectivity, but use the Admin status route for
inventory and management readiness details.

Supported client-facing routes include:

```http
GET /api/models/{namespace}/{repo}/revision/{revision}
GET /api/models/{namespace}/{repo}/tree/{revision}
GET /api/datasets/{namespace}/{repo}/revision/{revision}
GET /api/datasets/{namespace}/{repo}/tree/{revision}
GET|HEAD /{namespace}/{repo}/resolve/{revision}/{path}
GET|HEAD /datasets/{namespace}/{repo}/resolve/{revision}/{path}
```

Tree requests accept the query fields used by supported clients. File responses
support `HEAD`, byte ranges, and conditional requests needed by those clients. These
routes are documented for diagnosis and interoperability; ordinary automation should
still use the official client.

A metadata response is assembled by ModelKeep from a file list, never relayed from
upstream. Nothing upstream says about where its payload lives — a redirect, a Xet hash,
a signed URL — reaches a client, so no metadata answer can send a client around the
mirror.

## Choose the correct interface

- Use the download endpoint for a requested model or dataset download, metadata
  lookup performed by an HF client, and warm/offline retrieval.
- Use the Admin API for inventory, explicit prefetch or refresh, integrity
  verification, full archive audit, and asynchronous job monitoring.
- Use neither interface for deployment changes or archive deletion unless a separate,
  explicitly authorized procedure covers that action.

An Admin API `202 Accepted` response means that a job was queued, not that it
completed. Follow [`admin-api.md`](admin-api.md) through terminal job status.

## Failure interpretation

Client-facing status codes distinguish common failure classes:

- `400`: unsafe or malformed archive path;
- `401`: upstream authorization failed during a cold acquisition;
- `404`: the requested upstream object or revision does not exist;
- `416`: a requested byte range is unsatisfiable;
- `502`: upstream is unavailable, or the acquisition this request was waiting for did
  not complete — including one an operator cancelled;
- `503` on a `resolve` route, or on a repository metadata route where
  `MODELKEEP_METADATA_COLD_MISS_DEADLINE_SECONDS` is configured: the acquisition is
  still running and exceeded the cold-miss deadline; retry after `Retry-After`
  seconds, or prefetch instead;
- `507`: archive storage failure;
- `500`: integrity, helper-contract, publication conflict, or another internal
  failure.

Do not turn integrity or storage errors into cache misses, and do not bypass the
mirror after a failure. For an explicit management job, report the Admin API's safe
structured `error_class` and `message`; never expose raw upstream output or
credentials.
