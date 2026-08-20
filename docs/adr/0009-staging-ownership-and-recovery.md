# ADR-0009: Lease-protected staging ownership and recovery

- Status: Accepted
- Date: 2026-08-21

## Context

ModelKeep uses staging directories for fetches and publications. Recovery must remove abandoned work without deleting an operation that is still active in another process or container.

## Decision

Every staging operation receives an exclusive, collision-resistant directory name and a lease record containing a random operation nonce, process identity, and expiry. Recovery removes only staging directories with an expired valid lease; unrecognizable metadata is preserved for conservative manual inspection.

Read-only management commands do not run recovery as a side effect. Startup and explicitly mutating workflows may run recovery before beginning work.

The lease is an ownership hint and recovery safety boundary, not a sole PID check. A process crash leaves the lease to expire, after which a later recovery can remove the abandoned operation.

## Consequences

- Concurrent operations cannot reuse a staging directory name.
- Active long-running downloads are protected by a valid lease.
- Stale staging is eventually recoverable without automatic archive GC.
- Recovery is conservative when metadata is malformed or missing.
- The archive temporary path must remain writable and support directory rename on the same filesystem as the archive.
