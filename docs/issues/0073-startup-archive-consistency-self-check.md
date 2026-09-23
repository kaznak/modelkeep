---
status: open
priority: P2
related_adrs:
  - ADR-0011
  - ADR-0001
  - ADR-0007
created: 2026-09-24
updated: 2026-09-24
---
# Issue 0073: Check archive self-consistency at startup

- Status: Open
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
- The check runs from the packaged image, not only from a development shell.

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
