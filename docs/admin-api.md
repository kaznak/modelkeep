# ModelKeep Admin API

The versioned Admin API is the supported automation interface for inventory and
asynchronous management jobs. Browser UI actions use the same API. The management
origin is separate from the Hugging Face-compatible download origin.

For ordinary model or dataset downloads and guidance on choosing between the two
interfaces, see the [`ModelKeep client and API guide`](modelkeep-api.md).

Do not put a deployment hostname, bearer token, or operator identity in tracked
commands or agent instructions. Obtain the origin from an ignored site configuration
or an explicitly supplied environment variable:

```sh
MODELKEEP_ADMIN_ENDPOINT=${MODELKEEP_ADMIN_ENDPOINT:-$(
  jq -er '.admin_endpoint' qnap-acceptance.config.json
)}
export MODELKEEP_ADMIN_ENDPOINT
```

## Authorization and request rules

Every route requires either the configured bearer token or the trusted Tailscale
application capability described in
[`ADR-0015`](adr/0015-separate-management-control-plane.md). A client reaching the
approved Tailscale Service normally sends no authorization header itself. For an
explicit bearer deployment, read the token from a protected environment variable and
send `Authorization: Bearer ...`; never put it in a URL, file, log, or command history.

All state-changing requests require `X-ModelKeep-CSRF: 1`. Job submissions also
require:

```text
Content-Type: application/json
Idempotency-Key: <non-empty unique value, at most 128 bytes>
```

An idempotency key belongs to one principal and one exact request. Repeating that
request returns the original job. Reusing the key for different input returns
`409 idempotency_conflict`. Generate a new key for an intentional retry after a
terminal failure. Equivalent queued or running jobs are also deduplicated even when
their keys differ.

The normalized acquisition selection is part of "one exact request": a prefetch for the
same repository and revision under different patterns is a different request and a
different job, while two submissions whose patterns differ only in order or duplication
are the same request. A submission that carries no patterns is identified exactly as it
was before selections existed, so keys recorded by an earlier version keep matching.

HTTP `202 Accepted` means only that a new job was queued. It is not evidence that the
operation completed. Poll the returned job until a terminal state is observed.

## Read-only routes

### Service status

```http
GET /api/admin/v1/status
```

Reports the version, readiness, pull-through availability, repository counts, logical
archive bytes, archive filesystem capacity/low-space state, and authenticated
principal. Agents should check this before submitting work and stop if `ready` is
false or `pullthrough_enabled` is false for `prefetch` or `refresh`.

`self_check` carries the stored archive self-check result: its `status`
(`never_run`, `running`, `clean`, or `findings`), counts, and `findings_by_kind`.
This route never starts an archive walk, which is why the result is stored; the
findings themselves are on the self-check route below, and `self_check` reports no
staging size, because measuring one means walking a directory.

### Repository inventory

```http
GET /api/admin/v1/repositories?limit=50&cursor=<opaque>
GET /api/admin/v1/repositories/{repo_type}/{namespace}/{repository}
```

`repo_type` is `model` or `dataset`. List responses contain `items` and an optional
`next_cursor`; pass that cursor unchanged to retrieve the next page. The detail route
returns refs and immutable revisions, including file counts and logical bytes. A
missing repository returns `404 {"error":"not_found"}`.

The legacy model-only detail route without `{repo_type}` remains available, but new
automation should always include the repository type.

### Job history and status

```http
GET /api/admin/v1/jobs?limit=50&cursor=<opaque>
GET /api/admin/v1/jobs/{job_id}
```

The list is newest first and uses an opaque `next_cursor`. A job includes its state,
phase, target, acquisition selection, resolved commit when known, resume flag,
byte/file counters, timestamps, principal, terminal `outcome`, and safe failure
classification/message. States are `queued`, `running`, `completed`, `failed`, and
`cancelled`.

`queued` means the job holds no upstream transfer slot. An acquisition job stays
`queued` while it resolves (`phase` `resolving_revision`) and while it waits behind the
per-repository transfer gate (`phase` `waiting_for_transfer_slot`, ADR-0021), and
becomes `running` when its transfer starts. A job that is `queued` with a `started_at`
is therefore one that has begun work but is not yet transferring; it is visible and
cancellable throughout. `verify` and `audit` become `running` as soon as they start.

`include` and `exclude` report the normalized selection the job acquires; both are
empty for a whole-repository job.

`outcome` says what a completed acquisition did to the archive, so a job that had
nothing to transfer is distinguishable from one that transferred and moved no bytes:

- `already_archived`: every path the selection covers was already archived. Nothing was
  transferred; the byte and file counters stay `null` because no transfer was started.
- `published`: the revision was published for the first time.
- `extended`: an already published revision gained the paths it did not hold.

`outcome` is `null` for `refresh`, `verify`, and `audit`, for any job that did not
complete, and for records written before this field existed; its absence is not a
failure. It is stored in the durable job record itself, so it survives index
reconstruction and adds no other source of truth.

Poll one job rather than repeatedly scanning all history:

```sh
while :; do
  job=$(curl --fail --silent --show-error \
    "$MODELKEEP_ADMIN_ENDPOINT/api/admin/v1/jobs/$job_id") || exit
  printf '%s\n' "$job" | jq \
    '{state,phase,resumed,outcome,progress_bytes,total_bytes,progress_files,total_files,error_class,message}'
  state=$(printf '%s\n' "$job" | jq -r '.state')
  case "$state" in
    completed) break ;;
    failed|cancelled) exit 1 ;;
  esac
  sleep 15
done
```

Use bounded polling intervals and a task-appropriate elapsed-time limit. Do not infer a
stalled job solely from a low instantaneous transfer rate; large-file progress can be
bursty. Report the safe `error_class` and `message` on failure, not credentials or raw
upstream responses.

## Submit a job

```http
POST /api/admin/v1/jobs
```

Supported request bodies are:

```json
{"kind":"prefetch","repo_type":"model","repo_id":"org/repo","revision":"<ref-or-commit>"}
{"kind":"prefetch","repo_type":"model","repo_id":"org/repo","revision":"<ref-or-commit>",
 "include":["Qwen3-Coder-Next-Q4_K_M/*"],"exclude":["*.gguf.part"]}
{"kind":"refresh","repo_type":"model","repo_id":"org/repo","revision":"main"}
{"kind":"verify","repo_type":"model","repo_id":"org/repo","revision":"<commit>"}
{"kind":"audit"}
```

`repo_type` may also be `dataset`; omission retains legacy model behavior. `prefetch`,
`refresh`, and `verify` require both `repo_id` and `revision`. `audit` rejects target
fields and checks the entire archive.

### Acquisition selection

`prefetch` optionally takes `include` and `exclude`, arrays of patterns passed to the
official Hugging Face client as `allow_patterns` and `ignore_patterns`; ModelKeep does
not match them itself. Omitting both acquires the whole repository, which remains the
default ([`ADR-0020`](adr/0020-selection-scoped-revision-acquisition.md) decision 6).
Selecting a subset is what keeps a request for one quantisation directory from costing
the whole repository.

Patterns are untrusted input and are validated before anything is queued. A pattern is
rejected when it is empty, starts with `!` or `/`, contains a backslash or a control
character, has an empty, `.`, `..`, or `.cache` path component, or begins with a
`.modelkeep-` component. A trailing `/` is the official client's directory form and is
accepted. The remaining patterns are sorted and de-duplicated. A rejected
submission returns `400 {"error":"invalid_request"}` and the offending pattern is not
echoed back into the response, job record, or logs.

`include` or `exclude` on `refresh`, `verify`, or `audit` is rejected the same way
`audit` rejects target fields: `400 {"error":"invalid_request"}`.

`progress_bytes`, `total_bytes`, `progress_files`, and `total_files` describe the
selected set, so progress is measured against what is actually being transferred.

A filtered prefetch publishes a revision holding exactly the files it acquired. The
archive records what it holds and asserts nothing about upstream completeness, so a
later request for a path that revision does not hold consults upstream and extends the
same revision (`outcome` `extended`) instead of re-acquiring the repository. To archive
the rest of a repository, submit the same target without patterns.

### Transfer serialization

At most one acquisition transfers per repository at a time, keyed by repository type
and repository ID, and at most `MODELKEEP_MAX_TRANSFERRING_ACQUISITIONS` transfer
across all repositories — **2** by default, whole numbers of at least 1
([`ADR-0021`](adr/0021-one-transferring-acquisition-per-repository.md)). The effective
value is reported at startup as `startup_configuration.max_transferring_acquisitions`
and on `GET /api/admin/v1/acquisitions` as `transfer_limit`.

Waiting is FIFO among the acquisitions that could run; one whose repository is busy
does not hold a free slot against an unrelated repository. A waiting job stays `queued`.

Two consequences matter operationally. First, overlapping selections stop paying twice:
by the time a queued acquisition runs, it reconciles its selection against what the
archive now holds and transfers only the remainder, which is often nothing. Second,
head-of-line blocking is real and deliberate — a large acquisition delays everything
else for its repository and holds one of a small number of global slots. That is why
cancellation exists; use `GET /api/admin/v1/acquisitions` to see what holds each slot.

Resolve-only and metadata work is never gated, so a prefetch whose selection is already
archived is answered promptly even while another transfer for the same repository is
running.

Prefer an immutable 40-character commit for deterministic prefetch and verify tasks.
Use a mutable ref only when the requested operation is specifically to resolve or
refresh that ref. Before starting a potentially large transfer, inspect repository
metadata and obtain user authorization proportional to its expected storage/network
cost.

Example submission:

```sh
idempotency_key=$(cat /proc/sys/kernel/random/uuid)
request=$(jq -nc \
  --arg repo "$repo" \
  --arg revision "$revision" \
  '{kind:"prefetch",repo_type:"model",repo_id:$repo,revision:$revision}')

response=$(curl --fail --silent --show-error \
  -H 'Content-Type: application/json' \
  -H 'X-ModelKeep-CSRF: 1' \
  -H "Idempotency-Key: $idempotency_key" \
  --data "$request" \
  "$MODELKEEP_ADMIN_ENDPOINT/api/admin/v1/jobs")
job_id=$(printf '%s\n' "$response" | jq -er '.id')
printf 'job_id=%s\n' "$job_id"
```

### Cancellation

```http
DELETE /api/admin/v1/jobs/{job_id}
X-ModelKeep-CSRF: 1
```

A job can be cancelled whether it is queued or running, of any kind. The response is
`200` with the job record plus a `cancellation` field saying what the request did:

- `cancelled`: the job is now terminal `cancelled`. Its acquisition was stopped, its
  helper process was stopped and reaped, and nothing was published.
- `already_terminal`: the job had already finished, failed, or been cancelled. The
  returned record says which. This is an answer, not an error.
- `already_finishing`: the acquisition had already passed its publication point, so it
  completes and publishes. Nothing was cancelled. Poll the job for its terminal state.

`404 {"error":"not_found"}` means there is no such job.

Cancellation is an interruption, not a deletion. No archived revision is removed, and
the bytes the acquisition had already transferred are kept as resumable fetch staging
([`ADR-0017`](adr/0017-resumable-fetch-staging.md)), so a later acquisition for the
same target under the same or a narrower selection adopts them and transfers less than
a fresh start. Cleanup of that staging stays with the existing lease-expiry path.

A `verify` or `audit` job has no upstream acquisition to stop. Its record becomes
terminal `cancelled` immediately and is never overwritten, but the archive walk it had
already started runs to completion in this process and its result is discarded. The
job is cancelled; the reading it was doing is not interrupted.

Cancelling a job cancels the acquisition it is running. Because identical work is
collapsed by single-flight, that one acquisition may also be serving client download
requests; those requests are answered `502` as well. This follows from sharing the
transfer and is not separately configurable.

Cancellation and container interruption are different: after a restart, a previously
active job becomes a terminal interrupted failure, and any safe staging resume belongs
to a newly submitted job.

### Acquisitions in flight

```http
GET /api/admin/v1/acquisitions
```

Lists every upstream acquisition in flight, including those a client download request
started rather than a job. The response reports the transfer gate as well:

```json
{"transfer_limit":2,"transferring":1,"waiting":1,
 "items":[{"id":"acq-000000000007","repo_type":"model","repo_id":"org/repo",
           "requested_revision":"main","include":[],"exclude":[],
           "operation":"pull_through","state":"transferring","phase":"downloading",
           "transferred_bytes":10485760,"total_bytes":null,
           "started_at":1774310000,"cancelled":false}]}
```

- `transfer_limit` is the effective global limit on concurrent transfers
  ([`ADR-0021`](adr/0021-one-transferring-acquisition-per-repository.md)),
  `transferring` is how many hold a slot, and `waiting` how many are queued for one.
- `state` is `resolving` (asking upstream what a selection covers, which is never
  gated), `waiting_for_transfer_slot` (queued behind the gate, transferring nothing),
  or `transferring` (holding a slot). Together with `repo_id` this is what shows which
  acquisition is blocking a repository.
- `operation` is `pull_through` for a download or prefetch acquisition and `refresh`
  for a ref refresh.
- `include` and `exclude` are the normalized selection; both empty means the whole
  repository.
- `transferred_bytes` counts only bytes that advanced, on the same basis as the
  `acquisition_progress` event. `total_bytes` is `null` when upstream has not reported
  a size.
- `cancelled` is true for an acquisition that has been asked to stop and has not yet
  finished unwinding.

There is deliberately **no field saying who requested an acquisition.** The download
data plane carries no principal — [`ADR-0015`](adr/0015-separate-management-control-plane.md)
keeps identity on the management plane — so any attribution here would be a guess, and
a partial hint is worse than none for deciding whether to stop a multi-day transfer.

The identifiers are per-process and are not stable across a restart; they address an
acquisition that is running now, nothing more.

```http
DELETE /api/admin/v1/acquisitions/{acquisition_id}
X-ModelKeep-CSRF: 1
```

Stops an acquisition, whichever kind of request started it. The response is `200` with
`{"cancellation":"...","id":"..."}` where `cancellation` is `cancelled`,
`already_cancelled`, or `already_finished` (it had passed its publication point and
completes). `404 {"error":"not_found"}` means nothing is in flight under that
identifier — including an acquisition that has already ended.

Every client request waiting on the acquisition is answered `502`, and any management
job running it reaches terminal `cancelled`. Other acquisitions are unaffected, and
serving already archived files is unaffected.

## Retained fetch staging

```http
GET /api/admin/v1/staging
```

Lists the staging directories the archive's temporary area holds, which is what the
self-check counts as `orphaned_staging` plus any acquisition currently running. The
count alone cannot say which of them is which, and the reasons carry different
actions, so every entry names its own:

```json
{"items":[{"name":"fetch-abandoned-4bb1baf7182d415883bc6d0576a909d3",
           "retention":"resumable","removable":true,"adoptable":true,
           "repo_type":"model","repo_id":"org/repo","requested_revision":"main",
           "commit":"<40-character commit>","selection":[],
           "size_bytes":31138512896,"file_count":12,"size_complete":true,
           "age_seconds":25007,"lease_expires_in_seconds":0,
           "recovery_skipped_action":null,"recovery_skipped_io_kind":null}],
 "total_bytes":31138512896,"retained_by_kind":{"resumable":1}}
```

`retention` is one of:

- `active`: the lease has not expired, so a live acquisition owns the directory. It
  is not retained state and cannot be removed.
- `resumable`: identified staging recording a resolved commit, which recovery keeps
  **on purpose** so a later matching acquisition adopts its bytes
  ([`ADR-0017`](adr/0017-resumable-fetch-staging.md)). This is an asset, not
  garbage: `commit` and `selection` say what a prefetch would have to request to
  reuse it, and `size_bytes` says what resuming saves.
- `unreadable_lease`: the lease is absent or unrecognizable, which recovery
  preserves for manual inspection
  ([`ADR-0009`](adr/0009-staging-ownership-and-recovery.md)). Nothing adopts it, so
  `adoptable` is false however much identity it carries.
- `not_reclaimable`: this process's startup recovery attempted the entry and could
  not reclaim it. `recovery_skipped_action` and `recovery_skipped_io_kind` repeat
  what its `staging_recovery_skipped` event reported, which is what makes that event
  and the self-check's count reconcilable from the API alone.
- `stale`: the lease expired and nothing records a commit a resume could use.

`retained_by_kind` summarizes the classes the way the self-check summarizes finding
classes. A self-check finding's `path` is the entry's `name`, so the two routes join
on it without reading container logs.

`size_bytes` and `file_count` are the directory's **own measured** contents, not the
archive filesystem's free space. `size_complete` is false when part of the directory
could not be read, which makes `size_bytes` a lower bound. Measuring walks the
directory, so it happens on this explicit request and never on a status poll.

`name` is the entry's own name, bounded as untrusted text: a name under a shared
volume need not have been chosen by ModelKeep. `removable` is false when the name is
not one this service will act on, or when the entry is `active`.

```http
DELETE /api/admin/v1/staging/{name}
X-ModelKeep-CSRF: 1
```

Removes one named staging directory. The response is `200` with
`{"name":"...","retention":"...","size_bytes":N,"file_count":N}` describing what was
removed.

- `400 {"error":"invalid_request"}`: the name is not one ordinary component directly
  under the temporary area, or it does not name a directory. Traversal, absolute and
  nested names are refused, and the rejected name is not echoed back.
- `404 {"error":"not_found"}`: no such staging directory.
- `409 {"error":"staging_active","lease_expires_in_seconds":N}`: the lease has not
  expired, so a live acquisition owns it. Stop the acquisition first
  (`DELETE /api/admin/v1/acquisitions/{id}`) and retry after the lease expires.

Removal is explicit, destructive, and one directory per request. ModelKeep never
removes staging automatically, on a schedule, under disk pressure, or as a side
effect of another operation ([`ADR-0004`](adr/0004-no-automatic-archive-gc.md),
[`ADR-0007`](adr/0007-explicit-revision-deletion.md)). `models` and `datasets` are
unreachable from this route: it takes a name, not a path, and no published revision,
ref or manifest is affected by it. A successful removal logs `staging_removed`; a
refusal logs `staging_removal_refused`.

Removing `resumable` staging discards bytes a later acquisition would otherwise have
reused, and nothing warns twice. Read `commit`, `selection` and `size_bytes` first,
and prefer submitting a prefetch for that target when the recorded commit is still
the one you want: the acquisition adopts the bytes instead of transferring them
again.

## Archive self-check

```http
GET /api/admin/v1/self-check
POST /api/admin/v1/self-check
X-ModelKeep-CSRF: 1
```

`GET` returns the stored result, `POST` runs the check now and returns the fresh
one. Both answer with the `self_check` summary fields from the status route plus
`findings`:

```json
{"status":"findings","completed_at":1790249413,"duration_ms":22,
 "repositories_checked":2,"revisions_checked":402,"files_checked":403,
 "refs_checked":2,"staging_directories":3,"orphaned_staging_directories":3,
 "oldest_orphaned_staging_age_seconds":25007,"filtered_internal_paths":0,
 "finding_count":3,"findings_by_kind":{"orphaned_staging":3},
 "findings":[{"finding":"orphaned_staging","repo_type":"model",
              "repo_id":"org/repo","commit":"<commit>","path":"fetch-abandoned-...",
              "reference":null,"age_seconds":25007,
              "detail":"fetch staging is left behind in the temporary area"}]}
```

`status` is `never_run` before the first check and `running` while one is in flight;
`findings` is empty in both cases. `finding` is one of `invalid_manifest`,
`missing_file`, `size_mismatch`, `unsafe_path`, `dangling_ref`, `orphaned_staging`,
or `unreadable_archive`. For `orphaned_staging`, `path` is the staging directory's
name, which is the `name` the staging listing reports.

The check reads manifests and file metadata, never file contents, never contacts
upstream, and repairs nothing: no finding causes a deletion, a re-acquisition, or a
write to a published revision. `POST` walks the archive, so do not poll it; it
returns `409 {"error":"self_check_running"}` rather than starting a second walk, and
`duration_ms` from a previous run is how long the walk takes on this archive.
Digest verification remains the `verify` and `audit` jobs.

## Error handling

- `400`: malformed target, revision, cursor, job ID, or idempotency key; an unsafe
  acquisition pattern; a selection on a kind that does not acquire;
- `401`: missing/invalid bearer authorization or missing trusted Tailscale capability;
- `403`: state-changing request omitted `X-ModelKeep-CSRF: 1`;
- `404`: requested repository, job, or staging directory does not exist, or no
  acquisition is in flight under the given identifier;
- `409`: idempotency conflict; a staging removal whose lease has not expired
  (`staging_active`); a self-check that is already running (`self_check_running`);
- `500`: internal metadata/storage error.

A cancellation request never answers `409`. What it found is reported in the
`cancellation` field of a `200`, because "it had already finished" is an operational
answer and hiding it behind an error loses it.

For an asynchronous operation, the HTTP submission response and the terminal job
result are separate. Preserve that distinction in automation and user-facing reports.
