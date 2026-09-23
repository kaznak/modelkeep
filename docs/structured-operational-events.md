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
| `upstream_fetch_started` | INFO | `repo_id`, `requested_revision`, `operation`, `resumed`, `selected` |
| `upstream_fetch_finished` | INFO | fetch fields plus immutable `commit` |
| `upstream_fetch_failed` | WARN | fetch fields plus credential-safe `error_class` |
| `archive_verification_failed` | WARN | `repo_id` and immutable `commit`, or `requested_revision` and `operation`; credential-safe `error_class` |
| `archive_published` | INFO | `repo_id`, `requested_revision`, immutable `commit`, `operation` |
| `archive_extended` | INFO | `repo_id`, `requested_revision`, immutable `commit`, `added`, `skipped`, `operation` |
| `archive_selection_satisfied` | INFO | `repo_id`, `requested_revision`, immutable `commit`, `covered` |
| `archive_storage_failed` | ERROR | `repo_id`, `requested_revision`, `operation`, `error_class=storage`, `io_kind` |
| `admin_job_failed` | WARN | `job_id`, `job_kind`, job target (`repo_id`, `revision`), `error_class`, credential-safe `safe_reason` |
| `admin_server_ready` | INFO | `listen_address` |
| `admin_server_bind_failed` | ERROR | `listen_address`, `error` |
| `admin_active_job_skipped` | WARN | `job_id`; an unreadable marker also has `error` |
| `admin_job_progress` | INFO | `job_id`, `repo_type`, `repo_id`, `progress_bytes`, `total_bytes`, `progress_files`, `total_files` |
| `admin_job_persist_failed` | ERROR | `job_id`, `error` |
| `admin_job_index_skipped` | WARN | `path`; a malformed (as opposed to misidentified) job record also has `error` |
| `admin_job_index_update_failed` | ERROR | `job_id`, `index` (one of `by_created`, `idempotency`, `active`), `error` |
| `admin_archive_error` | WARN | `error` |
| `incomplete_fetch_preserved` | WARN | `repo_id`, `requested_revision` |
| `incomplete_fetch_recovered` | INFO | `repo_id`, `requested_revision`, `recovery_action`; resumable staging also has immutable `commit` |
| `acquisition_progress` | INFO | `request_kind`, `repo_id`, `requested_revision`, `path`, `phase`, `acquired_bytes`, `total_bytes` |
| `acquisition_deadline_exceeded` | WARN | `request_kind`, `repo_id`, `requested_revision`, `path`, `deadline_seconds`, `acquired_bytes` |
| `acquisition_abandoned` | ERROR | `repo_id`, `requested_revision` |
| `server_ready` | INFO | `listen_address` |
| `server_bind_failed` | ERROR | `listen_address`, `error` |
| `process_failed` | ERROR | `error` |
| `ownership_initialization_started` | INFO | `target`, `owner` |
| `ownership_initialization_failed` | ERROR | `target`, plus `error` when the ownership command could not run or `exit_status` when it ran and exited non-zero |
| `ownership_initialization_completed` | INFO | `target`, `owner` |
| `configuration_failed` | ERROR | `field`, `error`; `field=listen_address` also has `value` |
| `startup_started` | INFO | `version`, `archive_root`, `listen_address` |
| `archive_initialization_started` | INFO | `archive_root` |
| `archive_initialization_failed` | ERROR | `error` |
| `archive_initialization_completed` | INFO | none |
| `archive_recovery_started` | INFO | none |
| `archive_recovery_failed` | ERROR | `error` |
| `archive_recovery_completed` | INFO | `recovered_staging_directories` |
| `archive_readiness_failed` | ERROR | `error` |
| `startup_configuration` | INFO | `pullthrough_enabled`, `management_enabled`, `cold_miss_deadline_seconds`, `metadata_cold_miss_deadline_seconds` |
| `shutdown_started` | INFO | none |
| `shutdown_completed` | INFO | none |
| `health_probe_succeeded` | DEBUG | `endpoint` |
| `readiness_probe_succeeded` | DEBUG | `endpoint` |
| `readiness_probe_failed` | WARN | `endpoint`, `error` |
| `archive_self_check_started` | DEBUG | `archive_root` |
| `archive_self_check_finding` | WARN | `finding`, plus whichever of `repo_id`, `commit`, `path`, `reference`, `age_seconds` the class carries, and `detail` |
| `archive_self_check_completed` | INFO | `status`, `finding_count`, `revisions_checked`, `duration_ms` |

`request_kind` is one of `model_info`, `model_tree`, `get_file`, or `head_file`;
`model_info` and `model_tree` cover the dataset metadata routes as well, which are
distinguished by `repo_type`. Every repository event carries `repo_type`, which is
`model` or `dataset`. `operation` is the operation that failed or caused a
transition, such as `pull_through`, `refresh`, `stage`, `publish`, or `update_ref`.

`archive_extended` reports an acquisition that added paths to an already published
immutable revision rather than publishing a new one: `added` and `skipped` count the
manifest entries added and the ones the revision already held.
`archive_selection_satisfied` reports the opposite outcome, that the requested
selection needed no transfer, and `covered` counts the paths the selection resolved
to.

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
what distinguishes a live acquisition from a stalled one. Every route that can miss
reports this the same way, so `request_kind` says whether the acquisition was
started by a file request (`get_file`, `head_file`) or by a repository metadata
request (`model_info`, `model_tree`). A metadata acquisition has no single requested
file, so its `path` field is empty; a file acquisition carries the requested path.

A metadata acquisition is unbounded by default and so normally reaches publication
rather than a deadline: `acquisition_deadline_exceeded` appears with
`request_kind=model_info` or `model_tree` only where an operator configured
`MODELKEEP_METADATA_COLD_MISS_DEADLINE_SECONDS`
([`modelkeep-api.md`](modelkeep-api.md)). `acquisition_progress` is emitted for both
kinds regardless.

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

## `serve` startup sequence

`modelkeep serve` emits a fixed sequence of events before it starts accepting
connections; each step either emits its "started"/success event and continues, or
emits a failure event and aborts, so an operator can tell how far startup got.
Every event below except `startup_started` requires the previous step to have
succeeded (or, for the initial parse, is emitted instead of it):

1. The listen address is parsed. An invalid address emits `configuration_failed`
   with `field=listen_address` and aborts; a valid address does not emit a success
   event of its own and startup proceeds directly to step 2.
2. `startup_started` — guaranteed once the listen address parses.
3. `archive_initialization_started`, then either `archive_initialization_failed`
   (aborts) or `archive_initialization_completed`.
4. `archive_recovery_started`, then either `archive_recovery_failed` (aborts) or
   `archive_recovery_completed` with `recovered_staging_directories`.
5. Archive readiness is checked; failure emits `archive_readiness_failed` and
   aborts. Success emits no dedicated event; the archive self-check is spawned in
   the background at this point (see "Archive self-check" above) and startup
   continues without waiting for it.
6. Management (admin) configuration is read from the environment. An invalid
   management configuration aborts startup but is **not** reported through
   `configuration_failed` or any other event documented here — only the top-level
   `process_failed` below is emitted for it.
7. The cold-miss deadline configuration is read; failure emits
   `configuration_failed` with `field=cold_miss_deadline` and aborts.
8. `startup_configuration` — guaranteed once every prior step has succeeded;
   `pullthrough_enabled` and `management_enabled` report which optional
   subsystems are active for this process, and the two `*_deadline_seconds`
   fields are `0` when the corresponding deadline is unconfigured.
9. The HTTP listener(s) then emit `server_ready`/`server_bind_failed` (and, when
   the management API is enabled, `admin_server_ready`/`admin_server_bind_failed`
   from a separate listener) as documented in the event table.

Any error returned by a `modelkeep` subcommand — `serve` as above, or any other
subcommand (`list`, `show`, `import-hf-cache`, `init-ownership`, `refresh`,
`verify`, `remove`, `audit`, `self-check`, `health`, `ready`) — is reported once
at the top level as `process_failed` before the process exits with a non-zero
status. `process_failed` is therefore not specific to `serve` and can be the only
event a failing invocation of any subcommand produces.

`modelkeep init-ownership` emits `ownership_initialization_started`, then either
`ownership_initialization_completed` or `ownership_initialization_failed`.
`ownership_initialization_failed` carries `error` when the ownership command
(`/bin/chown`) could not be executed at all, or `exit_status` when it ran and
exited with a non-zero status; the two are mutually exclusive outcomes of the
same event.

## Management API job lifecycle

These events come from the management (admin) HTTP API's job manager
(`src/admin.rs`) and are only emitted when the management API is enabled
(`management_enabled=true` in `startup_configuration`).

`admin_server_ready` and `admin_server_bind_failed` mirror `server_ready` and
`server_bind_failed` for the management listener specifically.

At management-API startup, the job manager scans previously recorded job
markers. `admin_active_job_skipped` (WARN) is conditional: it appears once per
marker that names an invalid job ID, or once per marker whose job record could
not be read (in which case it also carries `error`); a startup with no such
markers emits neither. Separately, a one-time index migration emits
`admin_job_index_skipped` (WARN) once per pre-existing job record that is either
malformed JSON (carries `error`) or has an identity mismatch between its file
name and its recorded ID (no `error`); this only fires for archives migrating
from before the by-created/idempotency index existed, and is otherwise never
emitted.

`admin_job_progress` (INFO) is conditional on progress updates arriving for a
job that is still tracked as active; it is not emitted for every progress
update, only ones where the job manager could locate the job's in-memory state.

Persisting a job's state to disk is attempted on every state transition.
Failure to write the primary job record emits `admin_job_persist_failed`
(ERROR) and the in-memory state is rolled back. Even when the primary record is
persisted successfully, updating a secondary index (`by_created`,
`idempotency`, or `active`) can fail independently; each such failure emits its
own `admin_job_index_update_failed` (ERROR) with `index` naming which one, and
does not roll back the already-persisted primary record.

`admin_archive_error` (WARN) is emitted by several management API read routes
(status, repository listing, repository detail, job listing) whenever the
underlying archive query fails; the route then answers with HTTP 500. It carries
only `error` and no request-identifying fields beyond what the surrounding HTTP
access log (if any) provides.

## Operator examples

With JSON-aware tooling, select disk-full events using:

```sh
docker logs modelkeep 2>&1 | jq 'select(.fields.event == "archive_storage_failed" and .fields.io_kind == "out_of_space")'
```

To follow one repository without exposing credentials:

```sh
docker logs modelkeep 2>&1 | jq 'select(.fields.repo_id == "org/model")'
```
