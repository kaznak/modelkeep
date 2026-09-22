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
