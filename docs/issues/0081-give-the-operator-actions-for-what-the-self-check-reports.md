---
status: open
priority: P2
related_adrs:
  - ADR-0007
  - ADR-0017
created: 2026-09-24
updated: 2026-09-24
---
# Issue 0081: Give the operator actions for what the self-check reports

- Status: Open
- Priority: P2
- Related ADR: ADR-0007, ADR-0017

## Objective

Let an operator act on, and re-verify, what the startup self-check reports, without
resorting to `rm -rf` or a service restart.

## Problem

Issue 0073 added a detector with no actuator, and the gap showed up the first time the
detector found something real.

On 2026-09-24 the self-check reported seven orphaned fetch staging directories on the
deployment, the oldest about 2.7 days old, holding roughly 182 GB between them. Acting on
that required, in order: reading each directory's `.modelkeep-fetch.json` by hand to learn
which repository, revision and selection it belonged to; judging from that whether its bytes
were still worth resuming; and removing the chosen ones with `rm -rf` against paths under
`/data/tmp`. No supported command does any of it.

Re-verifying afterwards required either restarting the service or running
`modelkeep self-check` inside the container. The Admin API cannot trigger the check, and the
status route deliberately serves the stored result from startup, so the reported figures
stayed stale until one of those two things happened.

The judgement in the middle of that is the part worth supporting rather than leaving to a
shell loop. Retained staging is not garbage: identified staging is resumable, and since
ADR-0020 an unrestricted one can be adopted by any narrower request. On the deployment, two
of the three identified directories had recorded commits that still matched upstream's
current `main`, so their 29 GB was directly recoverable by submitting a prefetch — and
deciding that needed information no supported command exposes.

## What v0.4.10 changed, measured on the deployment

Re-measured on 2026-09-24 after the v0.4.10 container recreation, because Issues 0083 and 0087
changed what startup reclaims and so changed what is left for an operator.

| | v0.4.9 | v0.4.10 |
|---|---|---|
| staging directories | 4 | 3 |
| orphaned staging | 4 | 3 |
| oldest orphan, age | 8,753 s | 25,007 s |

One directory went. **The oldest did not: it is the same directory in both reports** — 8,753 s
before the v0.4.9 check at `1790233158` and 25,007 s before the v0.4.10 check at `1790249413`
both place its creation at `1790224405`, within rounding. It survived the restart.

That is most likely correct. `self_check_staging` counts every staging directory whose lease is
expired or absent, and after Issues 0083 and 0087 that set contains things whose right action
differs:

- `fetch-abandoned-*` holding a resolved commit, which recovery skips **on purpose** because
  ADR-0017 keeps it resumable. It is an asset, not garbage.
- staging whose lease is unreadable, which ADR-0009 keeps for inspection.
- an entry recovery could not reclaim, which Issue 0087 now names separately with
  `staging_recovery_skipped`.
- genuinely stale junk.

**So "3 orphaned staging" is not three of the same thing, and the count cannot say which.**

Two further facts the measurement pinned down.

**The detail exists and is not exposed.** `SelfCheckFinding` already carries `repo_type`,
`repo_id`, `commit`, `path`, `reference`, `age_seconds` and a `detail` string, and
`modelkeep self-check` prints the whole report including `findings`. The `/status` route serves
only the counts and `findings_by_kind`. So the operator's path to "which directories" today is
`docker exec` into the container, and there is no API route that returns a finding at all —
`/api/admin/v1/self-check` is a 404.

**There are now two partial views that do not line up.** `staging_recovery_skipped` names what
startup could not reclaim; the self-check counts what has no live lease. Neither is a subset of
the other, and an operator reconciling them has to read logs and run a CLI inside the container.

## Scope

- List retained fetch staging through the Admin API: repository, requested revision, resolved
  commit, selection, size, age, and whether it is identified and therefore adoptable.
- **Say why each one is retained**, because after Issues 0083 and 0087 the reasons are distinct
  in the code and carry different actions: resumable and deliberately kept, unreadable and kept
  for inspection, or not reclaimable. A listing that does not distinguish them reproduces the
  count's problem at greater length.
- Remove a named retained staging directory through the Admin API, one at a time, with the
  same authorization and CSRF requirements as any other state-changing management request.
- Trigger the self-check through the Admin API and report the fresh result.

## Do not

- remove anything automatically, on a schedule, or as a side effect of another operation
  (core invariant 4);
- remove staging that is active or whose lease has not expired;
- touch `models` or `datasets`; this is `tmp` only;
- let a status poll start an archive walk, which is why the stored result exists.

## Acceptance criteria

- Retained staging is listable with enough identity for an operator to decide between
  resuming and discarding, including whether a resume is possible at all, **and each entry
  says which kind of retention it is** rather than leaving that to be inferred from a
  directory name.
- What `staging_recovery_skipped` reported and what the self-check counts can be reconciled
  from the API alone, without reading container logs or running a CLI inside the container.
- A named staging directory can be removed through the API, and an active or unexpired one
  cannot.
- The self-check can be triggered and its fresh result read, and a status poll still does not
  start a walk.
- Removing staging never affects a published revision, and a test asserts the archive tree is
  unchanged across a removal.
- The documented operator procedure for the self-check's findings no longer contains `rm -rf`.

## Verification

```sh
nix develop -c cargo fmt --check
nix develop -c cargo clippy --all-targets --all-features -- -D warnings
nix develop -c cargo test --all-features
nix flake check
```

Tests for listing identified and unidentified staging, refusing an active one, removing a
named one with the archive tree unchanged, and triggering the check. Confirm against a
fixture archive carrying each staging shape.

## Risks and assumptions

Exposing staging identity through the management API exposes repository and revision names,
which that plane already reports for jobs and inventory, so it adds no new class of
information. The sharper risk is making removal easy enough to be used without the judgement
it needs; the listing has to make "this is resumable, and resuming it saves this much" visible
at the point of decision, not in a separate document.
