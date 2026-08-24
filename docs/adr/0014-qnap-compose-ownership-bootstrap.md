# ADR-0014: Bootstrap QNAP bind-mount ownership with a constrained init service

- Status: Superseded by ADR-0016
- Date: 2026-08-21

## Context

The QNAP Container Station GUI can create a host directory and a Compose Application,
but it does not provide a portable way to assign that directory to the numeric
UID/GID `10001:10001` used by ModelKeep. Requiring SSH for `chown` defeats the intended
copy-and-paste deployment path. Running the long-lived application as root would
weaken the established container boundary.

## Decision

The supported QNAP Compose deployment uses a short-lived init service before the
ModelKeep service. It runs the same pinned ModelKeep image as UID/GID `0:0`, with a
read-only root filesystem, all Linux capabilities dropped, and only `CAP_CHOWN`
restored. Its fixed entrypoint changes ownership of `/data` itself to `10001:10001`
and then exits.

The operation is intentionally non-recursive. It never changes published revisions,
refs, or other existing archive children. Compose starts the long-lived ModelKeep
service only after the init service exits successfully. ModelKeep remains
UID/GID `10001:10001`, read-only, capability-free, and bound only to host loopback as
required by ADR-0013.

The OCI image includes the pinned ownership utility used by the fixed Compose
entrypoint. No shell, Docker socket, variable interpolation, or mutable image tag is
required.

## Rationale

This limits elevated authority to one auditable operation and keeps it out of the
network-facing process. A non-recursive ownership change is sufficient for a new
QNAP directory while refusing to silently rewrite permissions across durable state.

## Alternatives considered

- Run ModelKeep as root: rejected because the network-facing process needs no root
  privileges.
- Require SSH or `chmod 777`: rejected as inconvenient or unsafe.
- Use a Docker-managed named volume: rejected because QNAP snapshots, backups, and
  restore drills need an explicit host share.
- Recursively chown `/data`: rejected because existing durable archive ownership may
  be intentional and a broad rewrite is difficult to recover from.

## Consequences

Each Compose start runs an idempotent ownership adjustment on the mount root. A new
empty QNAP directory becomes writable by ModelKeep without SSH. If existing children
have incompatible ownership or ACLs, initialization does not conceal that condition;
operators must review and repair it explicitly.

## Validation

Compose normalization must prove the init service uses the same pinned image, runs as
`0:0`, has only `CHOWN`, mounts the same archive, and gates ModelKeep with
`service_completed_successfully`. Image inspection must prove the fixed ownership
utility exists on amd64 and arm64.
