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

CI runs the filesystem-independent restore drill on native amd64 and arm64 Linux. It
does not certify unrecorded QNAP firmware, ACL, snapshot, or filesystem behavior.

## First deployment

1. Apply [qnap-permissions.md](qnap-permissions.md).
2. Use the Compose image `ghcr.io/kaznak/modelkeep:v0.2.1`, or edit the literal
   `image:` field to a different released or `sha-...` image tag, and record
   `docker image inspect` output. Do not use Compose variable-default expressions in
   a QNAP Container Station Application.
3. Run `docker compose config`; confirm both services mount only
   `/share/Services/modelkeep` at `/data`.
4. Run `docker compose up -d`; confirm the `modelkeep-init` container stops with exit
   code zero and the `modelkeep` container becomes healthy. A stopped initializer is
   the expected steady state, not a service failure. Container Station may summarize
   the Compose Application as "Other" because it includes this completed service;
   use the two per-container states to distinguish success from failure.
5. Configure Tailscale Serve as described in
   [qnap-tailscale-serve.md](qnap-tailscale-serve.md). Confirm loopback HTTP and
   tailnet HTTPS work, and direct LAN port 8090 does not.
6. Check `docker compose ps`, readiness, and structured logs. At the default
   `RUST_LOG=info`, expect ownership initialization, startup, archive recovery, and
   `server_ready` events. Successful health probes are intentionally quiet.
7. Import a small model, verify it, and complete the restore drill below.

Keep `HF_TOKEN` in the deployment environment or QNAP secret facility, never in
`/data`, backup metadata, recorded commands, or URLs.

For temporary probe diagnostics, change the Compose setting to `RUST_LOG=debug` and
recreate the service. Successful `/healthz` and `/readyz` requests then appear as
debug events. Restore `RUST_LOG=info` afterward to avoid a log entry for every
healthcheck. A failed readiness check remains visible as a warning at the default
level. Invalid `RUST_LOG` syntax is reported and falls back to `info`.

The Compose file fixes the container names to `modelkeep` and `modelkeep-init` so
they remain concise in Container Station. Those names permit one supported
ModelKeep deployment per Docker host. To run a second independent deployment, first
choose distinct `container_name` values, ports, and archive shares.

When startup is ambiguous in the GUI, inspect the initializer's state and logs. Exit
code `0` plus an `ownership_initialization_completed` event means initialization
succeeded. A non-zero exit code or `ownership_initialization_failed` event indicates
a mount or ACL problem. Do not keep the initializer running: it intentionally exits
so the root identity and `CHOWN` capability are absent from the long-running service.

## Backup and restore drill

Back up the entire `/data` boundary: models, manifests, refs, and tmp. Prefer a QNAP
point-in-time snapshot; otherwise stop the container during file-copy backup. Keep a
second backup outside the NAS failure domain.

Restore into a new empty share, never over production. Start the same pinned image
without `HF_TOKEN` or fetch-helper variables, run `modelkeep verify`, clear a test
client cache, block Internet access, and download an explicit restored commit. Record
the snapshot identity, image digest, verification output, and result.

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
