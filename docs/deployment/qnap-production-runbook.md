# QNAP production, backup, and recovery runbook

`/data` is durable state; containers, images, clients, and indexes are replaceable.
Never deploy `latest`. Record an immutable image tag and digest for every change.

## Site acceptance record

Complete this on the target NAS before production use:

```text
QNAP model and QTS/QuTS hero version:
Container Station version:
archive filesystem/share and ACL:
snapshot mechanism and retention:
external backup target and encryption owner:
ModelKeep image tag and digest:
restore drill date, repo, commit, result, operator:
```

Run the phased [QNAP client acceptance suite](qnap-client-acceptance.md) from the
GX10 to capture cold/warm/offline downloads, Range behavior, the LAN/Tailscale
boundary, and restart/reboot recovery in a machine-readable release-acceptance
record. The restored archive download is an optional DR phase in that same tool,
not a requirement for each ModelKeep release.

CI runs the filesystem-independent restore drill on native amd64 and arm64 Linux. It
does not certify unrecorded QNAP firmware, ACL, snapshot, or filesystem behavior.

The optional [interrupted-prefetch resume drill](qnap-resume-drill.md) deliberately
stops an in-progress acquisition to validate retained staging on the actual QNAP
filesystem. Run it in a maintenance window when first enabling resumable acquisition
or after changing storage/container behavior; it is not required for every release.

## First deployment

1. Apply [qnap-permissions.md](qnap-permissions.md).
2. Use the same literal image and archive path in `compose.init.yaml` and
   `compose.yaml`. The default image is `ghcr.io/kaznak/modelkeep:v0.4.11`; edit both
   files together for a different released or `sha-...` tag and record `docker image
   inspect` output. Do not use Compose variable-default expressions in a QNAP
   Container Station Application.
3. Create a temporary Container Station Application from `compose.init.yaml`. Start
   it and confirm `modelkeep-init` exits with code zero and logs
   `ownership_initialization_completed`. Do not continue after a non-zero exit.
4. Remove the completed initialization Application. Then create the normal
   Application from `compose.yaml`, start it, and confirm its sole `modelkeep`
   container becomes healthy.
5. Configure Tailscale Serve as described in
   [qnap-tailscale-serve.md](qnap-tailscale-serve.md). Confirm loopback HTTP and
   tailnet HTTPS work, and direct LAN port 8090 does not.
6. Check `docker compose ps`, readiness, and structured logs. The initialization
   Application logs the ownership event; at the normal service's default
   `RUST_LOG=info`, expect startup, archive recovery, and `server_ready` events.
   Successful health probes are intentionally quiet.
7. Import a small model, verify it, and complete the restore drill below.

Keep `HF_TOKEN` in the deployment environment or QNAP secret facility, never in
`/data`, backup metadata, recorded commands, or URLs.

For temporary probe diagnostics, change the Compose setting to `RUST_LOG=debug` and
recreate the service. Successful `/healthz` and `/readyz` requests then appear as
debug events. Restore `RUST_LOG=info` afterward to avoid a log entry for every
healthcheck. A failed readiness check remains visible as a warning at the default
level. Invalid `RUST_LOG` syntax is reported and falls back to `info`.

The two Compose files fix their container names to `modelkeep-init` and `modelkeep`.
The temporary initialization Application is removed before normal operation, leaving
only `modelkeep` visible. Those names permit one supported ModelKeep deployment per
Docker host. To run a second independent deployment, first choose distinct
`container_name` values, ports, and archive shares in both files.

The initialization Application is intentionally one-shot. Exit code `0` plus an
`ownership_initialization_completed` event means it can be removed. A non-zero exit
or `ownership_initialization_failed` event indicates a mount or ACL problem. It is
not rerun for an ordinary image upgrade that reuses the same archive, but it must be
run before ModelKeep uses a newly created or restored archive directory.

## Backup and restore drill

Back up the durable archive state under `/data`: published model files, their
manifests, and refs. Prefer a QNAP point-in-time snapshot; otherwise stop the
container during file-copy backup. Keep a second backup outside the NAS failure
domain.

`/data/tmp` contains incomplete acquisitions and is not required for archive
recovery. Exclude it from content-addressed or deduplicating backups such as restic;
otherwise a long-running acquisition can upload large amounts of temporary model
data that no completed backup needs. For example, when restic sees the archive at
`/data`:

```sh
restic backup /data --exclude='/data/tmp/**'
```

Adjust the excluded path when restic sees the archive through a different host or
container mount. Do not exclude published revision directories,
`.modelkeep-manifest.json` files, or refs. Run restic against a read-only QNAP
point-in-time snapshot where possible. A live filesystem scan is not itself a
point-in-time snapshot and may span a revision publication and ref update. If QNAP
snapshots are unavailable, stop ModelKeep while taking the backup.

Interrupting restic does not require deleting ModelKeep's active `/data/tmp`
contents. Re-run the backup with the exclusion in place; uploaded data that is not
referenced by any completed restic snapshot can be reclaimed later with `restic
prune` during a low-I/O maintenance window. If a completed restic snapshot includes
`/data/tmp`, keep it until its normal retention expiry unless the entire snapshot is
known to be disposable; restic cannot remove only one path from an existing
snapshot.

Restore into a new empty share, never over production. Start the same pinned image
without `HF_TOKEN` or fetch-helper variables, run `modelkeep verify`, clear a test
client cache, block Internet access, and download an explicit restored commit. Record
the snapshot identity, image digest, verification output, and result.

Optionally use the acceptance suite's `post-restore` phase for the empty-client
download and byte comparison after the server-side verification has succeeded. Run
this drill when first establishing the backup process, after storage/ACL/backup
configuration changes, and periodically according to the site's DR policy.

## Archive integrity audit

Run `modelkeep audit /data` from a scheduled one-shot container during the NAS's
lowest-I/O window. The command reads manifests and hashes every published file, so do
not overlap it with snapshots, RAID scrubs, or large model acquisitions. Start with a
monthly schedule and adjust only after measuring QNAP disk latency during serving.

Capture stdout as JSON and record the process exit status. `status: "clean"` with a
zero exit status means the complete run finished cleanly. `status: "failed"` and a
non-zero status identify revisions that failed verification. A killed container,
missing JSON output, or any other incomplete run is not a successful audit; schedule
a replacement run. The audit is read-only and never repairs or deletes data.

## Acting on the archive self-check

The self-check runs once at startup and reports; it repairs nothing. Do the whole
procedure through the Admin API. **Do not delete staging by hand with a recursive
shell removal against `/data/tmp`, and do not restart the service to refresh the
result** — those were the only options before the routes below existed, and both are
worse than the supported ones: a hand removal can destroy resumable bytes and can
reach paths the API refuses, and a restart re-reads the whole archive rather than
answering the question.

Read the origin as [`admin-api.md`](../admin-api.md) describes; never put the
hostname or token in a tracked command.

1. Read the stored result. `self_check.status` is `clean`, `findings`, `never_run`,
   or `running`:

   ```sh
   curl --fail --silent --show-error \
     "$MODELKEEP_ADMIN_ENDPOINT/api/admin/v1/status" | jq '.self_check'
   ```

2. Read the findings themselves. This is stored state too, not a new walk:

   ```sh
   curl --fail --silent --show-error \
     "$MODELKEEP_ADMIN_ENDPOINT/api/admin/v1/self-check" \
     | jq '.findings | group_by(.finding) | map({(.[0].finding): .})'
   ```

   A finding other than `orphaned_staging` concerns a published revision or ref: run
   `verify` for that revision, restore from backup if it fails, and do not delete
   anything on the strength of the finding alone.

3. For `orphaned_staging`, list the retained staging. Each finding's `path` is an
   entry's `name`, so the two views join on it:

   ```sh
   curl --fail --silent --show-error \
     "$MODELKEEP_ADMIN_ENDPOINT/api/admin/v1/staging" \
     | jq '.retained_by_kind, [.items[] | {name,retention,size_bytes,age_seconds,
            repo_id,commit,adoptable,removable,recovery_skipped_io_kind}]'
   ```

4. Decide per entry from its `retention`, because the right action differs:

   - `active`: a live acquisition owns it. Leave it. `GET
     /api/admin/v1/acquisitions` says what is running.
   - `resumable`: identified staging holding a resolved commit. **It is an asset.**
     If `commit` is still the revision you want, submit a `prefetch` for that
     `repo_id` and `commit` (whole repository, or the recorded `selection` or
     narrower): the acquisition adopts these bytes instead of transferring
     `size_bytes` again. Remove it only when the recorded commit is no longer wanted.
   - `unreadable_lease`: kept for inspection. Nothing will adopt it. Look at its
     contents before removing it; its identity may be wrong or absent.
   - `not_reclaimable`: startup recovery tried and failed.
     `recovery_skipped_io_kind` says why — `permission_denied` is usually a foreign
     uid under the share, `not_empty` a concurrent writer, `out_of_space` or
     `read_only` a storage problem to fix first. Fix that cause, then remove it.
   - `stale`: nothing records a commit a resume could use. Remove it.

5. Remove one entry, named, with CSRF:

   ```sh
   curl --fail --silent --show-error -X DELETE \
     -H 'X-ModelKeep-CSRF: 1' \
     "$MODELKEEP_ADMIN_ENDPOINT/api/admin/v1/staging/$name"
   ```

   `409 staging_active` means the lease has not expired: stop the acquisition with
   `DELETE /api/admin/v1/acquisitions/{id}` and retry after the lease expires (120
   seconds). `400 invalid_request` means the name is not one entry under
   `/data/tmp`; the route reaches nothing else, and `models` and `datasets` are not
   addressable through it. Removal is never automatic and never affects a published
   revision.

6. Re-verify without a restart:

   ```sh
   curl --fail --silent --show-error -X POST \
     -H 'X-ModelKeep-CSRF: 1' \
     "$MODELKEEP_ADMIN_ENDPOINT/api/admin/v1/self-check" \
     | jq '{status,finding_count,findings_by_kind,orphaned_staging_directories,
            oldest_orphaned_staging_age_seconds,duration_ms}'
   ```

   The walk costs I/O proportional to the archive, so run it after acting, not on a
   poll. `409 self_check_running` means one is already in flight. The status route
   then reports this fresh result.

## Management job history maintenance

Management job history is stored as one JSON record per job under
`/data/state/jobs`. ModelKeep keeps terminal history on disk and reads it by page; it
does not automatically delete these records. The `by-created` and `idempotency`
subdirectories are reconstructible indexes, while `active` identifies jobs that need
restart recovery.

If job-history disk or inode use eventually matters, stop the ModelKeep container,
inspect the selected top-level `<job-id>.json` records, and remove only confirmed
terminal-job records. Do not remove queued or running jobs, do not edit the `active`
directory while the service is running, and never apply the operation to `models` or
`tmp`. A later maintenance command may automate this workflow, but automatic history
retention is intentionally not part of the current service.

## Upgrade, rollback, and incidents

Before upgrade, snapshot storage and test the new pinned image against a restored
copy. Replace only the container and verify a known commit. Rollback selects the
previous image digest; it never edits or restores the archive merely for an app
rollback.

- Disk full: stop acquisition; add capacity or explicitly remove only a reviewed,
  unreferenced revision. Never run automatic GC. Check retained fetch staging first
  with `GET /api/admin/v1/staging`, which reports each directory's measured size:
  reclaiming a `stale` or no-longer-wanted directory there costs no archived data.
- Mount loss/read-only mount: stop the container and repair mount/ACL; do not accept a
  newly created empty `/data` as production.
- Interrupted acquisition: restart after storage repair; identified expired fetch
  staging can be resumed, while other recovery removes only safe expired staging.
  What a restart left behind is listed by `GET /api/admin/v1/staging`; act on it as
  "Acting on the archive self-check" above describes.
- Failed upgrade: retain logs, restore the previous image, run readiness and verify.
- Suspected corruption: stop writes, snapshot, verify read-only, and restore separately.
