# QNAP interrupted-prefetch resume drill

This optional drill validates ModelKeep's resumable acquisition optimization on the
actual QNAP filesystem and Container Station. It deliberately interrupts a prefetch;
run it only during a maintenance window. It does not replace the release acceptance
suite, and it does not modify a completed archived revision.

The commands below read the management origin from the Git-ignored site configuration
created for the QNAP acceptance suite. Do not paste an internal hostname into this
document or a tracked evidence file.

## Fixed test object

The current drill uses the public, non-gated snapshot below. Its two large shards total
about 6.18 GB, which is large enough to interrupt without consuming tens of gigabytes:

```text
Qwen/Qwen2.5-3B-Instruct@aa8e72537993ba99e69dfaafa59ed015b17504d1
```

Before starting, confirm that this commit is not already archived. If it is present,
choose another public, non-gated multi-file snapshot, record its immutable 40-character
commit and total size, and substitute both values throughout the evidence.

## Prepare local variables

Run these commands on the same tailnet client used for acceptance testing:

```sh
config=qnap-acceptance.config.json
admin_endpoint=$(jq -er '.admin_endpoint' "$config")
repo=Qwen/Qwen2.5-3B-Instruct
revision=aa8e72537993ba99e69dfaafa59ed015b17504d1

curl --fail "$admin_endpoint/api/admin/v1/status" | jq
```

Open the management UI derived from the same local configuration:

```sh
printf '%s/admin/\n' "$admin_endpoint"
```

Search its inventory for the repository and commit. Stop if the revision is already
complete. In QNAP File Station or Storage & Snapshots, note the space currently used by
the ModelKeep archive's `tmp` directory. The host archive path is site configuration;
do not add it to this repository.

## Start and interrupt the first job

Submit the commit-pinned prefetch. The distinct idempotency key is intentional:

```sh
first_job=$(
  curl --fail --silent --show-error \
    -H 'Content-Type: application/json' \
    -H 'X-ModelKeep-CSRF: 1' \
    -H 'Idempotency-Key: qnap-resume-drill-first' \
    --data "$(jq -nc --arg repo "$repo" --arg revision "$revision" \
      '{kind:"prefetch",repo_type:"model",repo_id:$repo,revision:$revision}')" \
    "$admin_endpoint/api/admin/v1/jobs" | tee /tmp/modelkeep-resume-first.json | jq -er '.id'
)
printf 'first job: %s\n' "$first_job"
```

Wait until the job is downloading and `tmp` usage has grown by at least several
hundred MiB. Record the job JSON and `tmp` usage. Then use Container Station to force
stop the ModelKeep container or Application. A graceful wait for the prefetch to finish
does not exercise crash recovery.

Keep ModelKeep stopped for at least 130 seconds. Leases are refreshed every 30 seconds
and expire after 120 seconds; starting sooner can correctly leave the staging active
and cause the retry to report a temporary conflict. After the wait, recreate or start
the same pinned v0.4.12 Application with the same `/data` mount.

## Retry and verify reuse

Wait for readiness, then confirm that the original job is an interrupted failure:

```sh
curl --fail \
  "$admin_endpoint/api/admin/v1/jobs/$first_job" | \
  tee /tmp/modelkeep-resume-first-after.json | \
  jq '{id,state,phase,error_class,resumed,progress_bytes,total_bytes}'
```

Expected values include `state: "failed"`, `phase: "interrupted"`, and
`error_class: "interrupted"`. Container Station logs should also contain an
`incomplete_fetch_recovered` event with `recovery_action: "preserved_for_resume"`.

Submit the same repository and immutable commit with a new idempotency key:

```sh
second_job=$(
  curl --fail --silent --show-error \
    -H 'Content-Type: application/json' \
    -H 'X-ModelKeep-CSRF: 1' \
    -H 'Idempotency-Key: qnap-resume-drill-second' \
    --data "$(jq -nc --arg repo "$repo" --arg revision "$revision" \
      '{kind:"prefetch",repo_type:"model",repo_id:$repo,revision:$revision}')" \
    "$admin_endpoint/api/admin/v1/jobs" | tee /tmp/modelkeep-resume-second.json | jq -er '.id'
)
printf 'second job: %s\n' "$second_job"
```

Poll without creating another job:

```sh
while :; do
  job=$(curl --fail --silent --show-error \
    "$admin_endpoint/api/admin/v1/jobs/$second_job") || exit
  printf '%s\n' "$job" | jq \
    '{state,phase,resumed,progress_bytes,total_bytes,progress_files,total_files,error_class}'
  state=$(printf '%s\n' "$job" | jq -r '.state')
  case "$state" in
    completed) break ;;
    failed|cancelled) exit 1 ;;
  esac
  sleep 15
done
```

During the retry, confirm all of the following:

- the second job reports `resumed: true` and passes through `resuming_snapshot`;
- `tmp` usage continues from the retained staging rather than allocating another
  approximately 6.18 GB copy;
- the second job completes and publishes only the immutable commit;
- Container Station logs show `upstream_fetch_started` with `resumed: true`;
- the retained fetch staging is gone after successful publication.

Keep the `/tmp/modelkeep-resume-*.json` client files with the private deployment record
if desired. They may identify the operator and must not be committed. Record
only the sanitized result, image version/digest, QNAP firmware, approximate retained
and peak `tmp` bytes, and pass/fail outcome in Issue 0056.
