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
2. Set a released or `sha-...` image tag and record `docker image inspect` output.
3. Run `docker compose config`; confirm only the intended share maps to `/data`.
4. Run `docker compose run --rm modelkeep ready`, then `docker compose up -d`.
5. Check `docker compose ps`, readiness, and structured logs.
6. Import a small model, verify it, and complete the restore drill below.

Keep `HF_TOKEN` in the deployment environment or QNAP secret facility, never in
`/data`, backup metadata, recorded commands, or URLs.

## Backup and restore drill

Back up the entire `/data` boundary: models, manifests, refs, and tmp. Prefer a QNAP
point-in-time snapshot; otherwise stop the container during file-copy backup. Keep a
second backup outside the NAS failure domain.

Restore into a new empty share, never over production. Start the same pinned image
without `HF_TOKEN` or fetch-helper variables, run `modelkeep verify`, clear a test
client cache, block Internet access, and download an explicit restored commit. Record
the snapshot identity, image digest, verification output, and result.

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
