# QNAP archive permissions

The Compose deployment runs ModelKeep as UID/GID `10001:10001` and mounts the durable archive at `/data`. The host share must allow that numeric identity to read and write the archive. The container root filesystem is read-only; `/tmp` is transient and is not durable archive state.

## GUI-first setup

For a new archive directory, paste `compose.init.yaml` into a temporary QNAP
Container Station Application and start it. The sole `modelkeep-init` service changes
ownership of the mounted `/share/Services/modelkeep` directory itself to
`10001:10001`, then exits. Confirm exit code zero and the
`ownership_initialization_completed` log event, then remove this Application. No SSH
setup is required for a new empty directory.

Next, paste `compose.yaml` into the normal Container Station Application and start
it. This Application contains only `modelkeep`, starts directly as non-root, and
should remain running and healthy. Ordinary image upgrades that reuse the same
archive do not require the initialization Application.

The ownership change is deliberately non-recursive. If an existing archive already
contains children with incompatible ownership, the initializer fails to conceal that
state; remove the temporary Application and review the archive separately before
changing durable data. Do not make the share world-writable and do not change the
long-running service to root.

If a QNAP shared-folder ACL prevents even the init service from accessing the bind
mount, use the QNAP shared-folder permission GUI to grant the Application's storage
path access. This is the exceptional ACL case, not the normal first-start procedure.

The archive directory must be writable for `models/`, `tmp/`, revision publication, mutable refs, and explicit revision deletion. Keep backups and snapshots outside the container lifecycle; the contents below `/data` are the durable state.

## Preflight

Use the same public image and volume path in `compose.init.yaml` and `compose.yaml`.
If either literal value is edited, edit it in both files before starting a large
import or fetch.

In Container Station, first confirm the temporary `modelkeep-init` Application
completed successfully. Remove it, start the normal Application, and confirm
`modelkeep` is healthy. From the QNAP host UI terminal or another supported command
facility, the equivalent sequence is:

```sh
docker compose -f compose.init.yaml up
docker inspect modelkeep-init --format '{{.State.Status}} {{.State.ExitCode}}'
docker compose -f compose.init.yaml down
docker compose up -d
curl --fail http://127.0.0.1:8090/readyz
```

An init failure indicates a bind-mount or QNAP ACL problem. A successful init followed
by an unhealthy ModelKeep service indicates a separate archive or application error.

The service remains non-root in `compose.yaml`:

```yaml
user: "10001:10001"
volumes:
  - /share/Services/modelkeep:/data
```

Only the separate initialization Application uses `user: "0:0"` and
`cap_add: [CHOWN]`; it has no ports and is removed before the service Application is
created.

The fixed container names `modelkeep` and `modelkeep-init` are intended for one
deployment per QNAP host. Rename them in their respective files together with the
port and archive share before creating any second independent deployment.
