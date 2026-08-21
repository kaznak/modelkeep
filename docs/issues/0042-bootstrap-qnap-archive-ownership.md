---
status: open
priority: P1
related_adrs:
  - ADR-0013
  - ADR-0014
created: 2026-08-21
updated: 2026-08-21
---
# Issue 0042: Bootstrap QNAP archive ownership from Compose

- Status: Open
- Priority: P1
- Related ADR: ADR-0013, ADR-0014

## Objective

Allow a QNAP Container Station Application created from `compose.yaml` to initialize
an empty `/share/Services/modelkeep` archive without requiring SSH.

## Problem

The application container correctly runs as UID/GID `10001:10001`, while a directory
created by QNAP is owned by a host identity that the GUI cannot map to that numeric
container identity. ModelKeep therefore exits with `Permission denied` before it can
create the archive layout.

## Acceptance criteria

- Compose mounts `/share/Services/modelkeep` at `/data`.
- A short-lived init service changes only the mount-root ownership to `10001:10001`
  and completes before ModelKeep starts.
- The init service is read-only, drops all capabilities except `CHOWN`, and never
  recursively changes existing archive contents.
- The long-running ModelKeep service remains UID/GID `10001:10001`, capability-free,
  read-only, and loopback-only.
- Compose contains no variable interpolation required by QNAP.
- The amd64 and arm64 OCI images contain the exact init executable path used by
  Compose.

## Verification

```sh
nix flake check
nix build .#packages.x86_64-linux.modelkeep-image
docker compose config
```

Inspect the built image and normalized Compose model, then exercise first start from
an empty root-owned bind directory where Docker is available.

## Risks and assumptions

The QNAP Docker daemon permits a root init container with `CAP_CHOWN` to change the
owner of the bind mount root. Existing child entries are deliberately untouched; an
existing inconsistent archive remains an explicit administrative repair.
