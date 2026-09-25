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
| `upstream_fetch_failed` | WARN | fetch fields plus credential-safe `error_class` and `safe_reason` |
| `upstream_metadata_answered` | INFO | `repo_id`, `requested_revision`, immutable `commit`, `files` |
| `upstream_file_list_recorded` | INFO | `repo_id`, immutable `commit`, `files` |
| `archive_verification_failed` | WARN | `repo_id` and immutable `commit`, or `requested_revision` and `operation`; credential-safe `error_class` |
| `archive_published` | INFO | `repo_id`, `requested_revision`, immutable `commit`, `operation` |
| `archive_extended` | INFO | `repo_id`, `requested_revision`, immutable `commit`, `added`, `skipped`, `operation` |
| `archive_selection_satisfied` | INFO | `repo_id`, `requested_revision`, immutable `commit`, `covered` |
| `archive_storage_failed` | ERROR | `repo_id`, `requested_revision`, `operation`, `error_class=storage`, `io_kind` |
| `archive_already_published` | WARN | `repo_type`, `repo_id`, `requested_revision`, `operation`, `error_class=conflict`, `published` (commit directory name only) |
| `archive_unsafe_path` | WARN | `repo_type`, `repo_id`, `requested_revision`, `operation`, `error_class=unsafe_path`, bounded `unsafe_path` |
| `archive_revision_referenced` | WARN | `repo_type`, `repo_id`, `requested_revision`, `operation`, `error_class=referenced`, `reference_count`, bounded `references` |
| `admin_job_failed` | WARN | `job_id`, `job_kind`, job target (`repo_id`, `revision`), `error_class`, credential-safe `safe_reason` |
| `admin_job_cancelled` | INFO | `job_id`, `job_kind`, job target (`repo_id`, `revision`), `previous_state` |
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
| `staging_recovery_skipped` | WARN | `staging` (bounded directory name only), `recovery_action`, `error_class=recovery_skipped`, `io_kind` |
| `staging_removed` | INFO | `staging` (bounded directory name only), `retention`, `size_bytes`, `file_count` |
| `staging_removal_refused` | WARN | `staging` (bounded directory name only), `error_class` (`staging_conflict` or `unsafe_path`); a refused live lease also has `lease_expires_in_seconds` |
| `fetch_staging_conflict` | WARN | `repo_type`, `repo_id`, `requested_revision`, `staging` (directory name only), `lease_expires_in_seconds`, `error_class=staging_conflict` |
| `acquisition_progress` | INFO | `request_kind`, `repo_id`, `requested_revision`, `path`, `phase`, `acquired_bytes`, `total_bytes` |
| `acquisition_deadline_exceeded` | WARN | `request_kind`, `repo_id`, `requested_revision`, `path`, `deadline_seconds`, `acquired_bytes` |
| `acquisition_abandoned` | ERROR | `repo_id`, `requested_revision` |
| `acquisition_cancelled` | WARN | `repo_id`, `requested_revision`, `operation`, `acquisition_id`, `acquired_bytes` |
| `transfer_slot_waiting` | INFO | `repo_id`, `requested_revision`, `operation`, `transferring`, `waiting`, `transfer_limit` |
| `transfer_slot_admitted` | INFO | `repo_id`, `requested_revision`, `operation`, `waited_ms`, `transfer_limit` |
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
| `startup_configuration` | INFO | `pullthrough_enabled`, `management_enabled`, `cold_miss_deadline_seconds`, `metadata_cold_miss_deadline_seconds`, `max_transferring_acquisitions` |
| `shutdown_started` | INFO | none |
| `shutdown_completed` | INFO | none |
| `health_probe_succeeded` | DEBUG | `endpoint` |
| `readiness_probe_succeeded` | DEBUG | `endpoint` |
| `readiness_probe_failed` | WARN | `endpoint`, `error` |
| `archive_self_check_started` | DEBUG | `archive_root` |
| `archive_self_check_finding` | WARN | `finding`, plus whichever of `repo_id`, `commit`, `path`, `reference`, `age_seconds` the class carries, and `detail` |
| `archive_self_check_completed` | INFO | `status`, `finding_count`, `revisions_checked`, `duration_ms` |

`request_kind` is one of `model_info`, `model_tree`, `model_refs`, `get_file`, or
`head_file`; `model_info`, `model_tree` and `model_refs` cover the dataset metadata
routes as well, which are distinguished by `repo_type`. Every repository event
carries `repo_type`, which is `model` or `dataset`. `operation` is the operation that
failed or caused a transition, such as `pull_through`, `refresh`, `stage`, `publish`,
or `update_ref`.

`archive_extended` reports an acquisition that added paths to an already published
immutable revision rather than publishing a new one: `added` and `skipped` count the
manifest entries added and the ones the revision already held.
`archive_selection_satisfied` reports the opposite outcome, that the requested
selection needed no transfer, and `covered` counts the paths the selection resolved
to.

`upstream_metadata_answered` says a repository metadata answer was **not** derived
from archived state: the revision was not archived, and its file list came from
upstream without acquiring anything (Issue 0074). It is the operator's marker that
the answer describes upstream rather than the archive, and it is never followed by
a publication of its own — no payload was transferred and nothing was written to
the archive. An archived revision is answered from the archive and emits
`archive_hit` instead, never this event. A metadata request that could not be
answered this way, because the fetch helper reported no per-file metadata, falls
back to acquiring the revision and is reported by the ordinary cold-miss
sequence below.

`upstream_file_list_recorded` says a published revision now knows the upstream
file list of its immutable commit. It follows `archive_published` or
`archive_extended` for the acquisition that learned it, at most once per
revision, and `files` counts the paths upstream reported for the whole commit —
not the archived subset, which `archive_extended.added` counts. The record is
internal archive state: it adds no manifest entry, is never served, and is never
re-recorded, because the file list of an immutable commit cannot change. A
revision published before the list was recorded, or imported from a client cache,
simply never emits this event and keeps reporting the archived set. A failure to
write it is reported by `archive_storage_failed` with
`operation=record_upstream_files` and leaves the published revision untouched and
serving.

`upstream_fetch_failed.error_class` is one of `unavailable`, `not_found`,
`unauthorized`, `rate_limited`, `client_failure`, `invalid_output`, `storage`,
`failed`, or `io`. The first five are the classes the fetch helper establishes for
itself: upstream could not answer, upstream said the repository or revision does
not exist, upstream refused the credentials, upstream refused because it was asked
too often, and the transfer or the official client itself failed.
`client_failure` is also the class for an exception the helper could not place,
because reporting `unavailable` for an authorization problem would be worse than
reporting that only the client's own words are known. `invalid_output` is the
helper's own contract failure, which includes a helper that failed without
reporting why. An acquisition stopped on request is not an upstream failure and
never appears here; it is reported by `acquisition_cancelled`.

`upstream_fetch_failed.safe_reason` is the one-line reason (Issue 0084): the
sanitized diagnostic the helper reported for this failure, prefixed by ModelKeep's
description of the class, or that description alone when the failure did not come
from a helper report. The helper sanitizes it in the helper, which is the only
place that holds the exception and the context needed to recognize a credential:
it reduces every URL to its scheme, host and port, redacts token-shaped and
authorization-header-shaped material, and bounds the result to one printable line.
ModelKeep bounds what it stores as untrusted input — unprintable characters
removed, whitespace collapsed, length capped — but never tries to re-derive that
judgement by pattern matching text it did not raise. Raw helper stdout and stderr
still never reach a log: helper stderr remains discarded, and an I/O failure is
reported by its kind rather than by a message that would carry local paths.

For an invalid fetch-helper contract, `admin_job_failed.error_class` is `upstream`
and `safe_reason` contains only ModelKeep's fixed description of the rejected
contract condition. Raw helper stdout and stderr are never included.

For an acquisition that failed for a reason its class does not give on its own,
`admin_job_failed.safe_reason` and the job record's `message` carry that reason
(Issue 0084) — the same sanitized text `upstream_fetch_failed.safe_reason`
reports, so the Admin API and the container log agree. This is what replaced
`upstream acquisition failed`, which said nothing an operator could act on.
The `error_class` of the job record stays at the granularity the Admin API
already defines (`upstream`, `not_found`, `authorization`, `integrity`, `storage`,
`unsafe_path`, `conflict`, `staging_conflict`, `referenced`, `cancelled`,
`interrupted`, `upstream_disabled`); the helper's finer class is in
`upstream_fetch_failed.error_class` and at the start of the reason.
`conflict` is a publication the archive refused, and `staging_conflict` is fetch
staging held by a running acquisition ("Fetch staging collisions and recovery"
below): a job that never published anything is never reported under `conflict`.
What keeps this text free of credentials is not that the error carries no
payload — it now does — but that the reason can only have come from the helper's
own sanitizer by way of ModelKeep's bounding: the only way to obtain one is to
derive it from an upstream error, so there is no route by which a raw helper line,
a signed URL or a header could become a job record's message. Raw helper stdout
and stderr are still never included.

For storage failures, `io_kind=out_of_space` identifies ENOSPC. Other I/O failures
use `io_kind=other`. ModelKeep never deletes an archived revision in response to
either event. Capacity measurements and threshold alerting are specified separately
by Issue 0004.

## Cold-miss acquisition liveness

A cold miss emits `archive_miss` and then reaches one of three outcomes, which is
what distinguishes a live acquisition from a stalled one. Every route that can miss
reports this the same way, so `request_kind` says whether the acquisition was
started by a file request (`get_file`, `head_file`) or by a repository metadata
request (`model_info`, `model_tree`, `model_refs`). A metadata acquisition has no single requested
file, so its `path` field is empty; a file acquisition carries the requested path.

A metadata acquisition is unbounded by default and so normally reaches publication
rather than a deadline: `acquisition_deadline_exceeded` appears with
`request_kind=model_info`, `model_tree` or `model_refs` only where an operator configured
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

## Cancellation and transfer slots

`acquisition_cancelled` (WARN) is emitted once per acquisition that an operator
stopped, by the acquisition itself as it stops, so the event records what happened
rather than what was asked for. A repeated cancellation request emits nothing more.
`acquired_bytes` is what had moved by then, and those bytes are kept: the same
acquisition also emits `incomplete_fetch_preserved` when its staging is retained for
a later retry (ADR-0017). Nothing is published, so no `archive_published` or
`archive_extended` follows. Because single-flight collapses identical work, one
`acquisition_cancelled` can be the answer to several waiting requests and to a
management job at the same time.

`transfer_slot_waiting` and `transfer_slot_admitted` (both INFO) bracket the
per-repository transfer gate (ADR-0021). Every transferring acquisition emits both,
including one admitted immediately, whose `transfer_slot_admitted.waited_ms` is `0`.
`transferring` and `waiting` are the counts at the moment the acquisition joined the
queue, and `transfer_limit` is the effective global limit, the same value
`startup_configuration.max_transferring_acquisitions` reports. A resolve-only or
metadata invocation emits neither event, because it is not gated.

`admin_job_cancelled` (INFO) is emitted when a management job's record reaches the
terminal `cancelled` state; `previous_state` is the state it was cancelled from, so a
job stopped while queued behind the gate is distinguishable from one stopped while
transferring. A cancellation request that found the job already terminal, or its
acquisition already past the publication point, emits nothing, because nothing
changed.

## Fetch staging collisions and recovery

Every `ArchiveError` an archive operation can fail with now leaves one record
(Issue 0085). `archive_verification_failed` and `archive_storage_failed` were the
only two; `AlreadyPublished`, `InvalidPath` and `ReferencedRevision` reached a
`_ => {}` arm and converted silently, so a prefetch could fail with
`error_class: "conflict"` and leave nothing in the log saying where the conflict
came from. The match is exhaustive, so a new variant cannot be added into
silence.

Each of the three names what it failed on, because that is the detail the
absence of which sent an operator to read the source: the commit directory that
already exists, the path that was rejected, and the refs still pointing at a
revision. A repository id, a revision and a file name all arrive in a request,
so those fields are rendered through the same bounding Issue 0084 applies to a
helper's message - unprintable characters replaced, whitespace collapsed,
truncated at 200 characters - which is what stops a crafted name from forging a
second log record. The archive root is not reported, as it is not reported by
`fetch_staging_conflict`.

`fetch_staging_conflict` (WARN) is emitted when an acquisition is refused because
fetch staging for the same repository, revision and selection is held by a lease
that has not expired: another acquisition is running, and ADR-0017 does not let a
second one resume its bytes from underneath it. `lease_expires_in_seconds` is how
long that refusal can still last, which is bounded by the 120-second lease.

It is deliberately not a publication conflict. Nothing is being published when it
happens, and the revision may exist nowhere, so `error_class=staging_conflict`
names the acquisition that holds the work rather than the archive (Issue 0083).
A management job refused this way carries the same class: its `error_class` is
`staging_conflict` and its `message` names staging, while `conflict` keeps its
older and narrower meaning of a publication the archive refused. The two are
separate classes because they send an operator to different places — one to the
acquisition that is running, one to the archive.

Staging left behind by a process that was killed emits nothing at the time — the
process is gone. It is reclaimed by whichever comes first:

- startup recovery, which renames an expired active marker holding a recorded
  commit to the adoptable `fetch-abandoned-*` form and reports
  `incomplete_fetch_recovered` with `recovery_action=preserved_for_resume`, or
  discards one with no recorded commit and reports `recovery_action=discarded`;
- the next acquisition of the same identity, which adopts the marker once its
  lease has expired and reports `upstream_fetch_started` with `resumed=true`.
  A restart that happens within the lease window leaves the marker for this path,
  which is why an acquisition never depends on recovery having run.

Neither path deletes retained data that records a commit: it is preserved by being
renamed. A marker whose lease is absent or unreadable is left alone by both, for
conservative manual inspection (ADR-0009), and `modelkeep self-check` counts it
under `orphaned_staging` in the meantime. Because those directory names begin with
a dot, `ls` and `du tmp/*` do not list them; `self-check` and
`archive_self_check_finding` are what report them.

`staging_recovery_skipped` (WARN) names one entry under the staging directory
that startup recovery could not reclaim; recovery then carries on with the
remaining entries (Issue 0087). It exists because the alternative was worse:
recovery was a single loop that propagated the first failed rename or removal out
of `serve`, so one entry the runtime user cannot remove — a share artifact such
as `@Recycle` appearing under the archive volume, a file left by an SMB or NFS
client under a different uid, `ENOTEMPTY` from a concurrent writer while the
directory is walked — emitted `archive_recovery_failed`, then `process_failed`,
and took a mirror that was serving correctly from durable state offline. None of
those is archive corruption and none threatens a published revision, so skipping
one scratch entry is strictly better than refusing to start.

`staging` is the entry's own name, bounded the way Issue 0085 bounds a path,
because a name under a shared volume need not have been chosen by ModelKeep; the
archive path it sits under is not reported, as `fetch_staging_conflict` does not
report it. `recovery_action` names what was attempted, where
`incomplete_fetch_recovered` names a completed action: `examine` is reading the
directory entry at all, `preserve_for_resume` is the rename into the adoptable
form or the flush that makes it durable, and `discard` is the removal. `io_kind`
is the failure's kind without the path it happened on, and for this event it is
one of `permission_denied`, `not_empty`, `read_only`, `out_of_space`, or `other`.

`error_class=recovery_skipped` is deliberately its own class. It is not
`storage`: nothing was being written to the archive, no published revision is
involved, and it sends an operator to one scratch entry rather than to disk
health. It is also not a reason to serve partial data — nothing about
publication changes, and the entry is left exactly as it lies for manual
inspection (ADR-0009). `modelkeep self-check` keeps counting it under
`orphaned_staging` until someone removes it, which is the standing record that it
is still there; this event is the notification that it happened.

Recovery still aborts when the staging directory itself cannot be read, because
then no entry was attempted at all; that is the one remaining
`archive_recovery_failed` from this step. `recovered_staging_directories` counts
the entries discarded and never the ones skipped, so a startup that skipped
something reports it only through this event.

`staging_removed` (INFO) is the one way staging leaves the temporary area other
than through recovery or an adoption: an operator named one directory through the
management API and it was removed (Issue 0081). It is INFO rather than DEBUG
because the removal is destructive and explicit, so it belongs in the record of
what was done to the deployment. `retention` is the class the entry was in when
it was removed — `resumable`, `unreadable_lease`, `not_reclaimable` or `stale` —
and `size_bytes` with `file_count` are what the directory held, measured before
it was removed. The entry's own name is reported, bounded as Issue 0085 bounds
one, and never the archive path it sat under. Nothing published is involved: the
route reaches only one entry directly under the temporary area, and no published
revision is reachable from it.

`staging_removal_refused` (WARN) names a removal that was not performed.
`error_class=staging_conflict` is the same class `fetch_staging_conflict` uses,
for the same reason: the lease has not expired, so a live acquisition owns the
directory, and `lease_expires_in_seconds` says how long that will remain true.
`error_class=unsafe_path` is a name that is not one ordinary component directly
under the temporary area — a traversal attempt, an absolute path, a nested path,
or a name carrying a control character. The refused name is reported bounded and
is never echoed into the HTTP response.

There is deliberately **no automatic removal event**, because there is no
automatic removal: staging is never removed on a schedule, under disk pressure,
or as a side effect of another operation (core invariant 4, ADR-0007). What
recovery reclaims at startup is still reported by `incomplete_fetch_recovered`
and `archive_recovery_completed`.

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

The same three events are emitted by a check an operator triggers through
`POST /api/admin/v1/self-check` (Issue 0081), which is the check running, not a
second kind of check. That route and `GET /api/admin/v1/self-check` also return
the findings themselves, so acting on one no longer requires reading these events
or running the CLI inside the container. The status route keeps reporting the
stored result and starts no walk of its own.

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
   `archive_recovery_completed` with `recovered_staging_directories`. A single
   staging entry that cannot be reclaimed no longer aborts this step: it emits
   `staging_recovery_skipped` and recovery continues, so `archive_recovery_failed`
   here now means the staging directory itself could not be read (Issue 0087).
5. Archive readiness is checked; failure emits `archive_readiness_failed` and
   aborts. Success emits no dedicated event; the archive self-check is spawned in
   the background at this point (see "Archive self-check" above) and startup
   continues without waiting for it.
6. Management (admin) configuration is read from the environment. An invalid
   management configuration aborts startup but is **not** reported through
   `configuration_failed` or any other event documented here — only the top-level
   `process_failed` below is emitted for it.
7. The transferring-acquisition limit is read; an invalid value emits
   `configuration_failed` with `field=max_transferring_acquisitions` and aborts.
   Then the cold-miss deadline configuration is read; failure emits
   `configuration_failed` with `field=cold_miss_deadline` and aborts.
8. `startup_configuration` — guaranteed once every prior step has succeeded;
   `pullthrough_enabled` and `management_enabled` report which optional
   subsystems are active for this process, the two `*_deadline_seconds`
   fields are `0` when the corresponding deadline is unconfigured, and
   `max_transferring_acquisitions` is the effective transfer limit (ADR-0021),
   reported whether or not it was configured explicitly.
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
