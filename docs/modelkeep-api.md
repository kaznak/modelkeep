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
obtain authorization appropriate to that cost. Repository metadata for a revision the
archive has never seen acquires the whole repository; a request for a single file the
archive does not hold acquires that file.

A client's own `--include` / `allow_patterns` does **not** narrow a first acquisition.
The client reads repository metadata before it requests any file, and metadata for a
revision the archive has never seen acquires the whole repository, so the filtering
happens after ModelKeep has already paid for everything. Measured against both pinned
clients: a filtered download of an unseen repository archives the full snapshot.

To archive a subset, submit a filtered `prefetch` through the [Admin API](admin-api.md)
and read it warm afterwards. Once a revision is published, a request for a path it does
not hold acquires that path alone and adds it to the same revision, so filtering works
from then on.

The archive records what it holds and asserts nothing about upstream completeness
([`ADR-0020`](adr/0020-selection-scoped-revision-acquisition.md)). A revision may
therefore hold a subset of the upstream repository — from a filtered prefetch through
the [Admin API](admin-api.md), from a single-file request, or from an imported cache.
Repository metadata reports exactly the archived set and never fabricates entries. A
request for a path a revision does not hold is a miss, not a `404` from the archive:
whether the path exists is upstream's answer, and when upstream has it the file is
added to that same revision rather than published as a second one. A published path is
never overwritten or removed.

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

### Repository metadata waits by default

`GET` on `/api/.../revision/{revision}` and `/api/.../tree/{revision}` for a revision
the archive has never seen waits for the whole repository acquisition to finish and
then answers normally. It is **not** bounded by the `resolve` deadline, and by
default it is not bounded at all. The request can therefore stay open for minutes or
hours on a large repository.

That is deliberate, and it rests on two measurements:

- Neither client applies its 10-second `resolve` read timeout to a metadata request.
  0.36.0 waited 30 s and 1.27.0 waited 12 s per metadata request, and both then
  completed the download. A metadata cold miss is slow, not broken.
- Neither client retries a bounded metadata answer. `503`, and equally `429`, `425`,
  `500` and `504`, each end the call on the first response: `snapshot_download`
  raises `LocalEntryNotFoundError` and `list_repo_tree` raises `HfHubHTTPError`.
  There is no status that buys a client-side retry here, unlike on `resolve`.

Bounding metadata by default would therefore make the first mirror of any repository
whose acquisition outlasts the deadline fail outright, which is ModelKeep's central
use. 1.27.0 requests `revision` and then `tree` during a download; 0.36.0 requests
`revision` only, so both routes behave the same way.

The practical consequence is a client that appears to hang while the mirror fills.
Do not read a large repository cold through the download endpoint: submit a prefetch
job through the [Admin API](admin-api.md), follow it to a terminal job state, and
then download, which is then a warm read.

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

It is the same digest the `tree` route reports as that file's `oid`. The whole
response, a `HEAD`, and a `206` for a byte range all carry the digest of the **whole
file**, and `If-None-Match` is compared against that same value, so a `304` means the
caller holds these bytes rather than merely a file of this length in this revision. No
`x-linked-etag` is sent; the supported clients accept the digest in `ETag` alone. The
measurement behind this shape, for both pinned client versions, is
[`docs/observations/hugging-face-content-validator-2026-09-24.md`](observations/hugging-face-content-validator-2026-09-24.md).

Two consequences follow, and both are intended:

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
