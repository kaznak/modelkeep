---
status: done
priority: P2
related_adrs:
  - ADR-0011
  - ADR-0001
  - ADR-0007
created: 2026-09-24
updated: 2026-09-24
---
# Issue 0073: Check archive self-consistency at startup

- Status: Done
- Priority: P2
- Related ADR: ADR-0011, ADR-0001, ADR-0007

## Objective

Run a self-check against the deployed archive at startup that is confined to the
archive's own contents, and report what it finds, so that a durable inconsistency is
visible to the operator instead of surfacing as a failed client request weeks later.

## Problem

Startup currently runs `recover_incomplete()` only (`src/main.rs:324`), which reclaims
abandoned fetch staging. Nothing looks at the archive itself.

`/readyz` does not fill that gap by design. ADR-0011 limits it to verifying the `models`
and `tmp` directories and writing a tiny probe file, and states explicitly that
readiness "does not guarantee free space for a model-sized operation or full archive
integrity".

So a durable inconsistency in the deployed archive — a manifest listing a file that is
no longer on disk, a size that no longer matches, a ref pointing at a revision that does
not exist, an unsafe path, staging left behind by a form of interruption the recovery
path does not cover — is detected only when a client happens to request the affected
object. This is the same gap Issue 0068 identified for tests: each layer is checked in
isolation, and the composed production artefact is not.

The check is confined to what the archive can answer about itself. Whether upstream has
a file the archive does not hold is not a self-consistency question; it is answered when
that path is requested (ADR-0020).

## Write scope

- a startup self-check over the archive root;
- structured operational events for its results, per
  `docs/structured-operational-events.md`;
- the Admin status route where a summary belongs;
- the Nix package and image definition so the check ships with the deployed artefact.

## Do not touch

- the meaning of `/healthz` and `/readyz` (ADR-0011);
- automatic repair, deletion, or re-acquisition of anything it finds
  (core invariant 4, ADR-0007);
- upstream access of any kind.

## Checks in scope

- every path listed in a manifest exists, and its size matches the recorded size;
- every archive path is safe and resolves inside the archive root;
- every ref resolves to a revision directory that exists and is servable;
- manifests parse and declare a repository type consistent with their location;
- fetch staging and leases left behind, reported by count and age.

Full digest verification is deliberately out of scope: recomputing sha256 over a
multi-terabyte archive at every start is not viable. Deep verification stays in the
existing `verify` and `audit` management jobs, which already walk manifests and digests
and produce a job record.

## Acceptance criteria

- The check never contacts upstream and never writes to a published revision.
- Nothing is repaired, deleted, or re-acquired automatically; findings are reported.
- Serving of healthy revisions does not wait for the check to finish, and the meaning of
  `/healthz` and `/readyz` is unchanged.
- Findings appear as structured events and as a summary on the Admin status route,
  including a zero-findings result so that "checked and clean" is distinguishable from
  "never checked".
- Startup cost is bounded and measured: the check reads manifests and file metadata, not
  file contents, and its duration is reported.
- A deliberately damaged fixture archive — missing file, size mismatch, dangling ref,
  unsafe path, orphaned staging — is detected, and a healthy fixture reports no
  findings.
- The check runs from the packaged binary that the image ships, not only from a
  development shell. The image itself is covered separately by the `modelkeep-image`
  check.

## Verification

```sh
nix develop -c cargo fmt --check
nix develop -c cargo clippy --all-targets --all-features -- -D warnings
nix develop -c cargo test --all-features
nix flake check
```

Add fixture archives for each damage class and for the healthy case. Measure startup
duration against an archive with a realistic revision count and record it. Confirm the
check completes with upstream unreachable.

## Risks and assumptions

The cost of the check grows with the number of revisions, so it must read metadata only
and must not become a reason to delay serving. If the measured startup cost is material
on the QNAP deployment, the check should run concurrently with serving and report when
it completes, rather than being reduced to a sample.

## Implementation status

Implemented on 2026-09-24. Verified on x86_64-linux with `cargo fmt --check`,
`cargo clippy --all-targets --all-features -- -D warnings`, `cargo test --all-features`,
and `nix flake check`, each with its exit status taken directly rather than through a
pipe. An independent reviewer checked each acceptance criterion against the code.

`Archive::self_check` runs off the serving path, reads manifests and file metadata only,
contacts no upstream, and repairs nothing. Findings appear as structured events and as
an Admin status summary that distinguishes a clean result from never having run.
Measured at 14-20 ms over 402 revisions. The `archive-startup-self-check` flake check
exercises it through the packaged binary.

While adding it, the client integration harness was found to hold the server's stderr in
a pipe it read only at teardown, with the baseline already using 61,076 of the 61,920
available bytes. One more startup log line would have blocked the server on its next
write. The harness now drains stderr continuously.

`nix flake check` omits aarch64-linux as an incompatible system, so the QNAP release
architecture is covered by the native GitHub Actions jobs, not by this run.

## Field verification (2026-09-24, v0.4.9)

The self-check ran at startup on the QNAP deployment and reported, through the Admin status
route:

```json
{"status":"findings","duration_ms":167,"repositories_checked":11,"revisions_checked":11,
 "files_checked":322,"refs_checked":8,"staging_directories":7,
 "orphaned_staging_directories":7,"oldest_orphaned_staging_age_seconds":232745,
 "filtered_internal_paths":6,"finding_count":7,
 "findings_by_kind":{"orphaned_staging":7}}
```

167 ms over 11 revisions and 322 files, so the cost is not a factor at this archive's size.

It found seven orphaned fetch staging directories, the oldest about 2.7 days old, and
repaired none of them — which is the intended behaviour, not a shortcoming. Cleanup is an
explicit operator action (core invariant 4). This is also the first finding the check has
produced on real state, and it is a true one: those directories are residue from interrupted
acquisitions and nothing else was reporting them.

No manifest, path, ref or size finding was reported, so the archive itself is self-consistent
by the checks in scope.
