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

Only a queued job can be cancelled. Running acquisition is not remotely cancelled by
this API. Cancellation and container interruption are different: after a restart, a
previously active job becomes a terminal interrupted failure, and any safe staging
resume belongs to a newly submitted job.

## Error handling

- `400`: malformed target, revision, cursor, job ID, or idempotency key; an unsafe
  acquisition pattern; a selection on a kind that does not acquire;
- `401`: missing/invalid bearer authorization or missing trusted Tailscale capability;
- `403`: state-changing request omitted `X-ModelKeep-CSRF: 1`;
- `404`: requested repository or job does not exist;
- `409`: idempotency conflict or job state does not permit the requested action;
- `500`: internal metadata/storage error.

For an asynchronous operation, the HTTP submission response and the terminal job
result are separate. Preserve that distinction in automation and user-facing reports.
