---
status: open
priority: P2
related_adrs:
  - ADR-0009
  - ADR-0017
created: 2026-09-24
updated: 2026-09-24
---
# Issue 0087: Isolate startup recovery per entry so one stuck directory cannot hold the mirror down

- Status: Open
- Priority: P2
- Related ADR: ADR-0009, ADR-0017

## Objective

Stop a single unremovable entry under the staging directory from aborting startup
recovery and crash-looping the container, so junk in a scratch directory cannot take the
whole mirror offline.

## Problem

`main.rs` step 4 runs `archive.recover_incomplete().map_err(…)?`. The recovery body is a
single loop that propagates with `?`, so the first `fs::remove_dir_all` or `fs::rename`
that fails aborts the rest of that startup's recovery and propagates out of `serve`. The
process then emits `archive_recovery_failed`, then `process_failed`, and exits non-zero.
Under `restart: unless-stopped` that is a crash loop.

The blast radius is disproportionate to the cause. One entry under `/data/tmp` that the
runtime user cannot remove is enough:

- a QNAP share artifact such as `@Recycle` or `.@__thumb` appearing under the archive
  volume;
- a file left by an SMB or NFS client under a different uid;
- `ENOTEMPTY` from a concurrent writer while the directory is being walked.

None of those are archive corruption, and none of them threaten a published revision.
All of them currently prevent the server from starting, which means a mirror that is
serving correctly from durable state stops serving because of a scratch directory.

Skipping one directory would be strictly better than refusing to start.

## How this was found, and what it is not

This was raised while implementing Issue 0083 as a candidate explanation for a
deployment observation: a `.fetch-active-*` marker that startup recovery never reclaimed,
with no `incomplete_fetch_recovered` event, while the self-check counted seven orphaned
staging directories where six were visible.

**That explanation was withdrawn, and this issue is not it.** An aborted recovery loop
cannot explain the observation, because the abort propagates out of `serve` and the
deployment was serving and answering submissions. `recover_incomplete()` must have
returned `Ok` and skipped the marker by decision. The marker's lease was also present,
parseable and expired at the end — the manual rename produced `resumed: true`, and the
abandoned branch refuses to adopt on an absent lease and reports an integrity mismatch on
an unparsable one — so neither a missing nor a corrupt lease was involved. The consistent
account is a restart inside the 120-second lease window, where recovery correctly skipped
a live lease and emitted nothing, followed by a read-only self-check hours later counting
the marker once its lease had expired. Issue 0083 removed the consequence by making the
acquisition path self-sufficient.

So the investigation found a different and worse failure mode than the one suspected, and
this issue is that one. It is not urgent: after Issue 0083 no acquisition depends on
recovery having run.

## Scope

- Isolate recovery per entry: a failure to reclaim one entry must not prevent the
  remaining entries from being attempted.
- Report each entry that could not be reclaimed, naming it, at a level an operator sees.
- `recover_incomplete()` returns `Ok` when it made partial progress, so startup
  continues.

## Do not

- delete or move anything recovery does not already delete or move. This issue changes
  error handling, not reclamation policy, and ADR-0009 keeps unrecognizable state for
  manual inspection;
- suppress the failure silently. An entry that cannot be reclaimed is operationally
  meaningful and must be reported;
- treat an entry that cannot be reclaimed as a reason to serve partial data. Nothing
  about publication changes.

## Acceptance criteria

- With one entry under the staging directory that cannot be removed, the server starts,
  serves, and reclaims every other reclaimable entry.
- The entry that could not be reclaimed is named in a structured event with its own
  class, and the startup self-check still counts it.
- A published revision remains downloadable across that startup, and no partial data
  becomes observable.
- A test drives the unremovable case rather than asserting the shape of the code.

## Verification

```sh
nix develop -c cargo fmt --check
nix develop -c cargo clippy --all-targets --all-features -- -D warnings
nix develop -c cargo test --all-features
nix flake check
```

## Risks and assumptions

**The test is the hard part and the reason this is not trivial.** Making
`fs::remove_dir_all` fail portably is not straightforward: the obvious approach of
removing write permission on the parent directory has no effect when the test process
runs as root, which it may in CI and in the Nix build sandbox. Candidate approaches
worth evaluating before implementing: injecting the failure at the filesystem-operation
boundary rather than in the filesystem; using a path shape the operation must reject; or
running the case only where a non-root uid is available and skipping deliberately
elsewhere with the skip recorded. A test that silently does not exercise the failure
would be worse than no test, because it would assert the fix without proving it.

The change also narrows what a non-zero exit from startup means. Today it includes
"could not reclaim one scratch entry"; afterwards it does not, which is the intent, but
any operational runbook that treats a recovery failure as fatal needs to agree.
