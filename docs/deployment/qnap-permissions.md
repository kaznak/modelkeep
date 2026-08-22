# QNAP archive permissions

The Compose deployment runs ModelKeep as UID/GID `10001:10001` and mounts the durable archive at `/data`. The host share must allow that numeric identity to read and write the archive. The container root filesystem is read-only; `/tmp` is transient and is not durable archive state.

## GUI-first setup

Paste `compose.yaml` into a QNAP Container Station Application and start it. The
short-lived `modelkeep-init` service changes ownership of the mounted
`/share/Services/modelkeep` directory itself to `10001:10001`, then exits. Container
Station starts the non-root ModelKeep service only after that succeeds. The expected
steady state is the `modelkeep-init` container stopped with exit code zero and the
`modelkeep` container running and healthy. Container Station may label the overall
Application "Other" because the initializer has completed. No SSH setup is required
for a new empty directory.

The ownership change is deliberately non-recursive. If an existing archive already
contains children with incompatible ownership, the init service fails to conceal
that state; stop the Application and review it separately before changing durable
data. Do not make the share world-writable and do not change the long-running service
to root.

If a QNAP shared-folder ACL prevents even the init service from accessing the bind
mount, use the QNAP shared-folder permission GUI to grant the Application's storage
path access. This is the exceptional ACL case, not the normal first-start procedure.

The archive directory must be writable for `models/`, `tmp/`, revision publication, mutable refs, and explicit revision deletion. Keep backups and snapshots outside the container lifecycle; the contents below `/data` are the durable state.

## Preflight

Using the public image already pinned by `compose.yaml` (or after editing its literal
image value when necessary), run the same image and volume configuration used for
deployment before starting a large import or fetch:

In Container Station, confirm `modelkeep-init` completed successfully and `modelkeep`
is healthy. From the QNAP host UI terminal or another supported command facility, the
equivalent checks are:

```sh
docker compose ps -a
docker inspect modelkeep-init --format '{{.State.Status}} {{.State.ExitCode}}'
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

Only the completed init service uses `user: "0:0"` and `cap_add: [CHOWN]`; it has no
ports and does not remain running.

The fixed container names `modelkeep` and `modelkeep-init` are intended for one
deployment per QNAP host. Rename them together with the port and archive share before
creating any second independent deployment.
