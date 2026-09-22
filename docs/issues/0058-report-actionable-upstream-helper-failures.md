---
status: in_progress
priority: P1
related_adrs:
  - ADR-0005
  - ADR-0015
created: 2026-09-22
updated: 2026-09-23
---
# Issue 0058: Report actionable upstream helper failures

- Status: In Progress
- Priority: P1
- Related ADR: ADR-0005, ADR-0015

## Objective

Make an upstream helper contract failure distinguishable in management job output and
structured logs without exposing helper output, credentials, or signed URLs.

## Problem

QNAP prefetch jobs for current public model repositories can fail immediately with
only `integrity: upstream invalid output`. The helper stderr is intentionally not
forwarded, and the parser currently collapses every output-contract failure into one
message, so operators cannot distinguish malformed JSON, an unsupported event, a
missing result, a malformed commit, or an empty snapshot.

## Acceptance criteria

- Every helper output-contract rejection carries a fixed, credential-safe reason.
- Helper output-contract rejection is classified as `upstream`, not archive
  `integrity`.
- The management job records that reason.
- A structured `admin_job_failed` event includes the job target, error class, and
  safe reason.
- No raw helper stdout/stderr is copied into logs or management state.

## Verification

```sh
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
```

Re-run a failing QNAP prefetch and confirm both the job message and container log
identify the rejected helper contract condition.

## Risks and assumptions

This improves diagnosis but does not by itself correct the underlying helper result.
The QNAP reproduction is required to identify and fix that separate cause.

## Verification outcome

The two repositories from the original report,
`google/gemma-4-26B-A4B-it@main` and
`google/diffusiongemma-26B-A4B-it@main`, were prefetched again on QNAP with
ModelKeep v0.4.2. Both acquisitions proceeded successfully, so the original
helper contract failure did not reproduce. This result records no deployment
hostname, internal URL, credential, or operator identity.

Automated tests therefore retain the failure path: every helper contract
rejection maps to a closed set of fixed safe reasons, and injected helper
stdout/stderr payloads cannot reach the returned error, management state, or
structured failure event.
