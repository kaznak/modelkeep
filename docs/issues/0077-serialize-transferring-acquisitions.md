---
status: done
priority: P1
related_adrs:
  - ADR-0021
  - ADR-0005
  - ADR-0020
created: 2026-09-24
updated: 2026-09-24
---
# Issue 0077: Serialize transferring acquisitions

- Status: Done
- Priority: P1
- Related ADR: ADR-0021, ADR-0005, ADR-0020

## Objective

Stop paying twice for the same bytes, and bound how much the uplink and the staging area
are asked to carry at once, without moving transfer scheduling into ModelKeep.

## Problem

The single-flight key includes the selection, so two requests for overlapping but
different selections of one repository run as two concurrent acquisitions and transfer
every shared file twice. On the deployment's measured ~6 Mbps uplink that is expensive.

Nothing limits concurrency across repositories either. Each acquisition is a
`snapshot_download` with eight download threads by default, and each holds its own
partial data in staging, so N concurrent acquisitions multiply both the number of
competing transfers and peak temporary capacity by N. Concurrency buys no throughput on a
saturated link; it only makes everything finish later, and it makes running out of space
more likely.

Verified against the pinned `huggingface_hub` 1.27.0: `snapshot_download` fixes its
selection when called, exposes no queue or handle, and offers no way to add files to a
download in progress. Any scheduling beyond "which invocation runs when" would have to be
ModelKeep's own. ADR-0021 declines to build that.

## Scope

Implement ADR-0021.

- One transferring acquisition per repository, keyed by repository type and repository ID.
- A configurable global cap on transferring acquisitions, default two.
- FIFO waiting, with management jobs waiting in their existing `queued` state so they stay
  visible and cancellable.
- The gate covers transferring invocations only. Metadata and resolve-only invocations,
  including Issue 0070's reconciliation, must not be gated.
- Single-flight stays below the gate: identical work is still collapsed, not queued.

## Do not touch

- per-file scheduling, batching, or fairness inside an invocation; that stays with the
  official client (ADR-0005, ADR-0003);
- the archive representation, publication boundary, or staging identity semantics.

## Acceptance criteria

- A second acquisition for a repository does not begin transferring while another is, and
  begins once that one finishes.
- Two overlapping selections transfer each shared file once. The second acquisition's
  transferred bytes are **measured**, not asserted, and are lower than a fresh acquisition
  of the same selection.
- Acquisitions for different repositories run concurrently up to the limit; with the limit
  set to one, a second repository's acquisition waits.
- A resolve-only or metadata invocation issued by a running acquisition is not blocked by
  the gate. A test covers the reconciliation path specifically, because that is where a
  self-deadlock would appear.
- A management job waiting on the gate is `queued`, visible through the Admin API, and
  cancellable while it waits.
- Identical requests are still collapsed by single-flight rather than serialized behind
  each other.
- What holds each slot, and what is waiting, is visible through the Admin API and the
  admin UI.
- The effective limit is reported at startup with the other settings.

## Verification

```sh
nix develop -c cargo fmt --check
nix develop -c cargo clippy --all-targets --all-features -- -D warnings
nix develop -c cargo test --all-features
nix flake check
```

Tests must cover the gate, the global limit including a limit of one, the reconciliation
path not deadlocking, byte-level measurement of the avoided duplicate transfer, and
cancelling a job that is queued behind the gate.

## Risks and assumptions

Head-of-line blocking is introduced deliberately: a long acquisition delays others for its
repository and holds one of a small number of global slots. That is only acceptable
alongside Issue 0076, and this issue should not land before it. A deadlock between the
gate and an acquisition's own metadata calls is the sharpest implementation risk, and the
test for it must exist before the gate is trusted.

## Implementation status

Implemented on 2026-09-24 after Issue 0076, as ADR-0021 requires. Verified on x86_64-linux
with `cargo fmt --check`, `cargo clippy --all-targets --all-features -- -D warnings`,
`cargo test --all-features`, and `nix flake check`, each with its exit status taken
directly. No assertion in the diff was removed or altered.

One transferring acquisition per repository, and at most
`MODELKEEP_MAX_TRANSFERRING_ACQUISITIONS` — default two — overall, with the effective value
reported at startup. The permit is taken only around the transfer, and reconciliation runs
before it, so an acquisition cannot wait on a gate it holds. Single-flight is unchanged and
sits below the gate. What holds each slot and what is waiting is visible through
`/api/admin/v1/acquisitions` and the admin UI.

A test found a real defect during implementation: an acquisition admitted after waiting
used the difference it had computed *before* waiting, so two overlapping selections each
transferred the shared file. Reconciling again after admission fixed it. Measured, the
second of two overlapping acquisitions moves 200 bytes where a fresh one moves 600, and the
shared file transfers once.

Two existing concurrency tests rendezvoused inside the fake fetcher on a barrier, which
assumed the concurrent same-repository transfers ADR-0021 now forbids. Their
synchronisation moved to waiting for registration instead; their assertions, including the
`calls` counts, are unchanged, and both were run fifteen times without flaking.

Cancelling a `verify` or `audit` job terminates its record immediately, but the scan itself
runs to completion in-process and its result is discarded, because the archive walk is not
interruptible. This is documented in `docs/admin-api.md`; making the walk interruptible
would require changes in `src/lib.rs`.

Waiters on the gate poll their cancellation token every 50 ms; admission itself is
immediate via a condvar, so the poll affects only how quickly a waiting acquisition notices
it was cancelled.
