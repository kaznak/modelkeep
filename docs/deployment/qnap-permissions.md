# QNAP archive permissions

The Compose deployment runs ModelKeep as UID/GID `10001:10001` and mounts the durable archive at `/data`. The host share must allow that numeric identity to read and write the archive. The container root filesystem is read-only; `/tmp` is transient and is not durable archive state.

## Host-side setup

Create the dedicated share directory and grant only the required owner/group access. On a QNAP shell with POSIX ownership available, an administrator can use:

```sh
sudo mkdir -p /share/LLM/modelkeep
sudo chown 10001:10001 /share/LLM/modelkeep
sudo chmod 0750 /share/LLM/modelkeep
```

If the share is governed by QNAP ACLs, apply the equivalent read/write permission to the service identity through the shared-folder ACL UI. Verify the effective numeric ownership with `ls -ldn /share/LLM/modelkeep`; ACLs must not override the required write access. Do not make the share world-writable and do not change the container to root.

The archive directory must be writable for `models/`, `tmp/`, revision publication, mutable refs, and explicit revision deletion. Keep backups and snapshots outside the container lifecycle; the contents below `/data` are the durable state.

## Preflight

Using the public image already pinned by `compose.yaml` (or after editing its literal
image value when necessary), run the same image and volume configuration used for
deployment before starting a large import or fetch:

```sh
docker compose run --rm modelkeep ready
```

A successful command confirms that the container identity can access the required archive directories and create, sync, and remove a tiny probe file under `tmp/`. A failure should be treated as a permissions or mount problem and fixed before moving model data. This probe does not modify published revisions.

The service remains non-root in `compose.yaml`:

```yaml
user: "10001:10001"
volumes:
  - /share/LLM/modelkeep:/data
```
