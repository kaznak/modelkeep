---
status: open
priority: P1
related_adrs:
  - ADR-0017
  - ADR-0009
created: 2026-09-24
updated: 2026-09-24
---
# Issue 0083: Release expired active fetch staging

- Status: Open
- Priority: P1
- Related ADR: ADR-0017, ADR-0009

## Objective

Stop a killed acquisition from permanently blocking every later acquisition of the same
identity, and stop reporting that block as a publication conflict it is not.

## Problem

Staging acquisition refuses a request when an in-flight marker matches its identity:

```rust
// src/lib.rs, acquiring fetch staging
if read_fetch_staging_metadata(&path).is_ok_and(|metadata| {
        metadata.repo_type == repo_type
            && metadata.repo_id == repo_id
            && metadata.requested_revision == requested_revision
            && metadata.files == files
}) {
    return Err(ArchiveError::AlreadyPublished(path));
}
```

There is no lease check on that path. The abandoned-staging branch a few lines above does
check `read_lease_expiry` and only refuses while the lease is live, so the two are
asymmetric: an expired `fetch-abandoned-*` is adopted, while a `.fetch-active-*` refuses
forever, because nothing renames or expires it once the process that owned it is gone.

Observed on the deployment on 2026-09-24 with v0.4.9. A filtered prefetch of
`GZGavinZhao/Leanstral-1.5-119B-A6B-GGUF` at commit `929d4958…` was running when the
container was force-stopped. It left
`/data/tmp/.fetch-active-4bb1baf7182d415883bc6d0576a909d3` holding 8.94 GB of retained
data. Startup recovery did not reclaim it — no `incomplete_fetch_recovered` event was
emitted — and every resubmission of that repository, revision and selection then failed in
zero seconds with `error_class: "conflict"`, `"archive publication conflict"`. Twice,
deterministically.

The only way out was to rename the directory by hand:

```sh
mv .fetch-active-4bb1baf7182d415883bc6d0576a909d3 fetch-abandoned-4bb1baf7182d415883bc6d0576a909d3
```

After which the next submission adopted it and reported `resumed: true` with
`progress_bytes: 8943738646`. So the resume machinery works; it was unreachable.

Two further problems compound it.

**The message is wrong.** A staging collision returns `ArchiveError::AlreadyPublished`, so
an operator is told the archive has a publication conflict. Nothing was being published,
and no revision of that repository exists. The word points at the wrong subsystem.

**It is invisible.** `du -hs tmp/*` does not list a dot-prefixed directory, so the
directory holding 8.94 GB and blocking the work did not appear in the obvious listing. The
startup self-check counted it — seven orphaned staging directories where six were visible —
and that discrepancy was the only hint it existed.

## Scope

- Give the active-marker path the same lease reasoning the abandoned path has: an in-flight
  marker whose lease has expired is not in flight.
- Reclaim or rename an expired active marker during startup recovery, so a killed
  acquisition becomes adoptable without manual filesystem surgery.
- Report a staging collision as what it is, distinct from a publication conflict, both in
  the error and in the job record's class.

## Do not

- adopt staging whose lease is genuinely live; a running acquisition must still be refused;
- weaken the identity match that keeps a resume from adopting the wrong bytes;
- delete retained data as part of unblocking. Renaming to the adoptable form preserves it,
  which is the whole point of ADR-0017.

## Acceptance criteria

- An acquisition whose process died leaves staging that a later acquisition of the same
  identity adopts, with no manual step, once the lease has expired.
- A live lease still refuses a second acquisition of the same identity.
- Startup recovery reclaims or renames an expired active marker and says so in an event.
- A staging collision is reported with its own error class and a message that does not say
  publication.
- A test kills an acquisition, waits past the lease, and asserts the next one resumes and
  transfers fewer bytes than a fresh start — measured, not asserted.

## Verification

```sh
nix develop -c cargo fmt --check
nix develop -c cargo clippy --all-targets --all-features -- -D warnings
nix develop -c cargo test --all-features
nix flake check
```

The existing `archive-crash-upgrade` check exercises crash and resume but did not catch
this, because it recreates the container and its recovery path differs from a bare
force-stop followed by the same identity being resubmitted. Extend it or add a check for
that sequence specifically.

## Risks and assumptions

The lease is the only thing distinguishing "running" from "dead", so its expiry has to stay
conservative: adopting staging from a process that is merely slow would be worse than
refusing one from a process that is dead. The existing 120-second expiry with 30-second
refresh is the contract, and this issue is about honouring it on a path that currently
ignores it.
