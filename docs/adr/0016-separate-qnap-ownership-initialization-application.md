# ADR-0016: Separate QNAP ownership initialization from the service Application

- Status: Accepted
- Date: 2026-08-24
- Supersedes: ADR-0014

## Context

ADR-0014 put a short-lived ownership initializer and the long-running ModelKeep
service in one Compose Application. This enabled GUI-only deployment while keeping
root and `CAP_CHOWN` out of the network-facing process. However, QNAP Container
Station continues to show the successfully completed initializer beside the running
service and may summarize the Application as "Other", obscuring its steady state.

Starting the long-running container as root and dropping privileges in an entrypoint
would remove the extra service, but would weaken the inspectable container boundary:
the configured container user and some auxiliary processes would still be root.

## Decision

The supported QNAP deployment uses two independent Compose files and Applications:

1. `compose.init.yaml` is run once for a new archive share. It contains only the
   constrained `modelkeep-init` service from ADR-0014 and is removed after it exits
   successfully.
2. `compose.yaml` contains only the long-running `modelkeep` service and is deployed
   after initialization.

The initializer remains UID/GID `0:0`, read-only, without ports, with all Linux
capabilities dropped except `CAP_CHOWN`. It changes only the `/data` mount root to
`10001:10001`; the operation remains non-recursive. The service Application starts
directly as `10001:10001`, drops all capabilities, and never contains a root process.

Initialization is required for a new or replacement archive directory, but not for
ordinary image upgrades that reuse the same archive. If initialization fails, the
operator must not deploy the service Application. If it is skipped, ModelKeep's
archive readiness checks reject an unwritable archive rather than serving it as
ready.

## Rationale

This preserves the security boundary of ADR-0014 while giving Container Station a
single running service with an unambiguous steady state. The additional step occurs
only when provisioning storage and is possible entirely through the QNAP GUI.

## Consequences

Operators must paste and run the initialization Compose before the service Compose,
verify exit code zero, and remove the initialization Application. The two files must
pin the same image and archive path. Changing either value requires changing it in
both files. Restoring into a newly created directory requires initialization before
starting ModelKeep; upgrading only the image does not.

## Validation

Compose checks must prove that each file has exactly one service, both use the same
pinned image and archive mount, the initializer has only `CAP_CHOWN`, and the normal
service is `10001:10001` with all capabilities dropped. Native amd64 and arm64 image
checks must continue to prove that `modelkeep init-ownership` is available.
