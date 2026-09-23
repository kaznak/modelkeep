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

- `GET` and `HEAD` on `/{namespace}/{repo}/resolve/{revision}/{path}` wait for the
  acquisition for at most the cold-miss deadline, **8 seconds** by default. The
  deadline applies to both methods identically.
- If the file is archived inside that window, it is served normally.
- Otherwise ModelKeep answers `503 Service Unavailable` with a `Retry-After` header
  holding the deadline in seconds. The acquisition is **not** cancelled: it keeps
  running, and a retry joins it instead of starting a second download. A retry after
  a `503` therefore costs nothing upstream.
- `MODELKEEP_COLD_MISS_DEADLINE_SECONDS` configures the deadline in whole seconds.
  `0` restores an unbounded wait, which holds the connection open with no response
  headers for the whole transfer and is not recommended.

The default is set by the supported clients' own patience, measured against
`huggingface_hub` 0.36.0 and 1.27.0: both abandon a `resolve` metadata request after
10 seconds with a read timeout, which is the `status=000` that motivated this
contract. Answering at 8 seconds keeps ModelKeep's status inside that window. Both
versions retry a `503` on their own — 0.36.0 with its own 1s/2s/4s/8s/8s backoff,
1.27.0 following `Retry-After` — so a cold miss that completes within roughly a
minute is transparently recovered by the client.

Beyond that, the client gives up while the acquisition continues. Do not read a
large repository cold through the download endpoint. Submit a prefetch job through
the [Admin API](admin-api.md), follow it to a terminal job state, and then download,
which is then a warm read.

`503` from this route always means "acquiring, retry"; it never means the file is
absent, and no partial data is published or served because a request was cut off.
Operationally, an `archive_miss` event is followed either by `acquisition_progress`
events carrying a byte count that actually advanced, or by
`acquisition_deadline_exceeded`; a byte counter that is not moving is never reported
as progress.

Repository metadata routes (`/api/.../revision/...` and `/api/.../tree/...`) are not
bounded by this deadline yet, so a cold metadata lookup for a revision the archive
has never seen can still hold its connection for the whole repository acquisition.

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
- `502`: upstream is unavailable or acquisition failed;
- `503` on a `resolve` route: the acquisition is still running and exceeded the
  cold-miss deadline; retry after `Retry-After` seconds, or prefetch instead;
- `507`: archive storage failure;
- `500`: integrity, helper-contract, publication conflict, or another internal
  failure.

Do not turn integrity or storage errors into cache misses, and do not bypass the
mirror after a failure. For an explicit management job, report the Admin API's safe
structured `error_class` and `message`; never expose raw upstream output or
credentials.
