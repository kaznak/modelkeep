---
status: open
priority: P1
related_adrs:
  - ADR-0015
  - ADR-0017
  - ADR-0009
created: 2026-09-24
updated: 2026-09-24
---
# Issue 0076: Cancel a running acquisition

- Status: Open
- Priority: P1
- Related ADR: ADR-0015, ADR-0017, ADR-0009

## Objective

Let an operator stop an acquisition that is already running, whether a management job or
a client request started it, without discarding the bytes it has already transferred.

## Problem

`JobManager::cancel` refuses anything that is not queued (`src/admin.rs:882`), so a
running acquisition cannot be stopped through the Admin API. Client-driven pull-through
acquisitions are worse off: they have no job record at all, so there is nothing to
address. The only way to stop either is to stop the container, which also stops serving
everything else.

Observed on 2026-09-24: a prefetch of `Qwen/Qwen3-Coder-Next-GGUF` — 469.92 GB, about
seven days on the deployment's ~6 Mbps uplink — was started without a selection and
could not be stopped remotely.

This makes an ordinary mistake expensive and irreversible. Forgetting `--include` on a
request that reaches an unarchived repository commits the link for days. It also shapes
other decisions: Issue 0074 weighs reporting a revision's true upstream file list
against the risk that an unfiltered client download then acquires everything the archive
does not hold. That risk is only unacceptable while it cannot be stopped.

Issue 0069 moved client-driven acquisitions onto dedicated threads registered in the
single-flight map, so for the first time there is a handle to address them by.

## Scope

- Cancel a running management job, not only a queued one.
- List acquisitions that are in flight, including those started by a client request, with
  enough identity to choose one: repository, revision, selection, and bytes so far.
- Cancel an in-flight acquisition by that identity.
- Expose both in the admin UI, next to the job list.

## Acceptance criteria

- A running prefetch cancelled through the Admin API reaches a terminal cancelled state,
  and its helper process is stopped rather than left running.
- No partial data is published, and the staging is left resumable under ADR-0017, so a
  later acquisition with the same or a narrower selection adopts it and transfers fewer
  bytes than a fresh start. A test measures that reduction rather than asserting it.
- A client-driven acquisition in flight is listable and cancellable, and the clients
  waiting on it receive an operationally meaningful status rather than a hang, a false
  `404`, or a success that delivers nothing.
- Cancelling one acquisition does not disturb any other acquisition, and does not affect
  serving of archived files.
- A cancel that races with completion leaves consistent state: either the acquisition
  completed and published, or it is recorded cancelled, never both and never neither.
- Cancelling something that has already finished is reported as such, not as an error
  that hides what happened.
- The admin UI can cancel a running job and an in-flight acquisition.

## Verification

```sh
nix develop -c cargo fmt --check
nix develop -c cargo clippy --all-targets --all-features -- -D warnings
nix develop -c cargo test --all-features
nix flake check
```

Add tests for cancelling a running job, cancelling a client-driven acquisition with
waiters attached, the cancel/completion race, and a cancel followed by a resumed
acquisition that transfers measurably less. Confirm no helper process survives a
cancellation.

## Risks and assumptions

Stopping the helper mid-write must not leave staging that a later acquisition mistakes
for complete work; the existing publication boundary and staging identity are what
prevent that, and the tests must exercise them rather than assume them. Cancellation is
an interruption, not a deletion: nothing removes archived data, and staging cleanup stays
with the existing lease expiry path.
