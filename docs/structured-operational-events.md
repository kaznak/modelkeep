# Structured operational events

ModelKeep writes newline-delimited JSON logs to standard output. The `event` field is
the stable selector for operational automation; the human-readable `message` is not
an API. Event fields identify repositories and revisions, but never contain request
headers, bearer tokens, signed URLs, or upstream error payloads.

## Event contract

| Event | Level | Stable correlation and classification fields |
|---|---|---|
| `archive_request` | INFO | `request_kind`, `repo_id`, `requested_revision`; file requests also have `path` |
| `archive_hit` | INFO | request fields plus immutable `commit` |
| `archive_miss` | INFO | request fields |
| `upstream_fetch_started` | INFO | `repo_id`, `requested_revision`, `operation`, `resumed` |
| `upstream_fetch_finished` | INFO | fetch fields plus immutable `commit` |
| `upstream_fetch_failed` | WARN | fetch fields plus credential-safe `error_class` |
| `archive_verification_failed` | WARN | `repo_id` and immutable `commit`, or `requested_revision` and `operation`; credential-safe `error_class` |
| `archive_published` | INFO | `repo_id`, `requested_revision`, immutable `commit`, `operation` |
| `archive_storage_failed` | ERROR | `repo_id`, `requested_revision`, `operation`, `error_class=storage`, `io_kind` |
| `admin_job_failed` | WARN | `job_id`, `job_kind`, job target (`repo_id`, `revision`), `error_class`, credential-safe `safe_reason` |
| `incomplete_fetch_preserved` | WARN | `repo_id`, `requested_revision` |
| `incomplete_fetch_recovered` | INFO | `repo_id`, `requested_revision`, `recovery_action`; resumable staging also has immutable `commit` |
| `acquisition_progress` | INFO | `request_kind`, `repo_id`, `requested_revision`, `path`, `phase`, `acquired_bytes`, `total_bytes` |
| `acquisition_deadline_exceeded` | WARN | `request_kind`, `repo_id`, `requested_revision`, `path`, `deadline_seconds`, `acquired_bytes` |
| `acquisition_abandoned` | ERROR | `repo_id`, `requested_revision` |
| `archive_self_check_started` | DEBUG | `archive_root` |
| `archive_self_check_finding` | WARN | `finding`, plus whichever of `repo_id`, `commit`, `path`, `reference`, `age_seconds` the class carries, and `detail` |
| `archive_self_check_completed` | INFO | `status`, `finding_count`, `revisions_checked`, `duration_ms` |

`request_kind` is one of `model_info`, `model_tree`, `get_file`, or `head_file`.
`operation` is the operation that failed or caused a transition, such as
`pull_through`, `refresh`, `stage`, `publish`, or `update_ref`.

`upstream_fetch_failed.error_class` is one of `unavailable`, `not_found`,
`unauthorized`, `invalid_output`, `storage`, `failed`, or `io`. The upstream diagnostic itself
is intentionally excluded because helper output can contain credentials or signed
URLs. Detailed helper diagnostics are available only through their separately
redacted diagnostic path.

For an invalid fetch-helper contract, `admin_job_failed.error_class` is `upstream`
and `safe_reason` contains only ModelKeep's fixed description of the rejected
contract condition. Raw helper stdout and stderr are never included.

For storage failures, `io_kind=out_of_space` identifies ENOSPC. Other I/O failures
use `io_kind=other`. ModelKeep never deletes an archived revision in response to
either event. Capacity measurements and threshold alerting are specified separately
by Issue 0004.

## Cold-miss acquisition liveness

A cold miss emits `archive_miss` and then reaches one of three outcomes, which is
what distinguishes a live acquisition from a stalled one.

`acquisition_progress` reports byte movement only. A repeated counter is never
reported as progress: the event is emitted when `acquired_bytes` exceeds the
highest value already observed for that acquisition, so a stalled transfer falls
silent instead of producing a constant stream. `phase` is the fetch helper's own
phase name, and `total_bytes` is absent when the helper does not know the size.

`acquisition_deadline_exceeded` says the bounded wait ended and the response was
answered without the file; `deadline_seconds` is the configured bound and
`acquired_bytes` is what had moved by then. The transfer is not cancelled and is
not restarted by a retry, so this event is not a failure of the acquisition.

`acquisition_abandoned` is an internal failure: the acquisition ended without a
result. It is never a cache miss and never a partial result.

## Archive self-check

The self-check runs once at startup, beside serving, and emits one
`archive_self_check_finding` per finding and exactly one
`archive_self_check_completed`. The completion event is emitted with
`status=clean` and `finding_count=0` as well, so "checked and clean" is
distinguishable from "never checked"; the Admin status route reports the same
result under `self_check`, whose `status` is `never_run` until the first check
completes and `running` while one is in flight.

Every restart pays for the completion line, so it carries the result and the
measurement only. The full counts — repositories, files, refs, staging
directories, and findings by class — are on the Admin status route and in the
report `modelkeep self-check` prints, neither of which a restart writes to a
log. `archive_self_check_started` exists for the same reason at DEBUG: a check
in flight is answered by the status route without logging.

`archive_self_check_finding.finding` is one of `invalid_manifest`,
`missing_file`, `size_mismatch`, `unsafe_path`, `dangling_ref`,
`orphaned_staging`, or `unreadable_archive`. `detail` is ModelKeep's own
description of archive state and never contains helper output, request headers,
or credentials. A manifest path that fails validation is never echoed back: the
finding names the manifest entry by position instead, because a path ModelKeep
refuses to use is untrusted input everywhere else too.

A manifest entry naming one of ModelKeep's own internal archive paths, such as
`.cache/huggingface/download.json` left by an old writer, is not a finding.
Serving filters those paths, so no client can observe or request one; the check
counts them as `self_check.filtered_internal_paths` on the status route.

The check reads manifests and file metadata only, never file contents, and never
contacts upstream. It reports and repairs nothing: no finding causes a deletion,
a re-acquisition, or a write to a published revision (core invariant 4,
ADR-0007). `duration_ms` beside `revisions_checked` is what makes the startup
cost observable as the archive grows. Digest verification stays in the `verify`
and `audit` jobs.

Recovery emits `recovery_action=preserved_for_resume` when an expired, identified
download is retained for a later retry, and `recovery_action=discarded` when an
identified incomplete staging directory cannot be resumed and is removed. Invalid
or unidentifiable staging remains governed by the conservative recovery rules in
ADR-0017 and cannot safely supply repository correlation fields.

## Operator examples

With JSON-aware tooling, select disk-full events using:

```sh
docker logs modelkeep 2>&1 | jq 'select(.fields.event == "archive_storage_failed" and .fields.io_kind == "out_of_space")'
```

To follow one repository without exposing credentials:

```sh
docker logs modelkeep 2>&1 | jq 'select(.fields.repo_id == "org/model")'
```
