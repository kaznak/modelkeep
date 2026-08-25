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
boundary, restart/reboot recovery, and the restored archive download in a single
machine-readable record.

CI runs the filesystem-independent restore drill on native amd64 and arm64 Linux. It
does not certify unrecorded QNAP firmware, ACL, snapshot, or filesystem behavior.

## First deployment

1. Apply [qnap-permissions.md](qnap-permissions.md).
2. Use the same literal image and archive path in `compose.init.yaml` and
   `compose.yaml`. The default image is `ghcr.io/kaznak/modelkeep:v0.4.1`; edit both
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

Use the acceptance suite's `post-restore` phase for the empty-client download and
byte comparison after the server-side verification has succeeded.

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

## Upgrade, rollback, and incidents

Before upgrade, snapshot storage and test the new pinned image against a restored
copy. Replace only the container and verify a known commit. Rollback selects the
previous image digest; it never edits or restores the archive merely for an app
rollback.

- Disk full: stop acquisition; add capacity or explicitly remove only a reviewed,
  unreferenced revision. Never run automatic GC.
- Mount loss/read-only mount: stop the container and repair mount/ACL; do not accept a
  newly created empty `/data` as production.
- Interrupted acquisition: restart after storage repair; lease recovery removes only
  expired staging.
- Failed upgrade: retain logs, restore the previous image, run readiness and verify.
- Suspected corruption: stop writes, snapshot, verify read-only, and restore separately.
