---
status: done
priority: P2
related_adrs:
  - ADR-0018
created: 2026-09-24
updated: 2026-09-24
---
# Issue 0071: Distinguish a no-op prefetch from an empty transfer in the job record

- Status: Done
- Priority: P2
- Related ADR: ADR-0018

## Objective

Make a terminal prefetch job record state whether the job transferred nothing because
the revision was already archived, or because acquisition produced no bytes.

## Problem

Submitting a prefetch for an already archived repository completes within the same
second, with `resolved_commit` correctly populated and `progress_bytes`,
`total_bytes`, `progress_files`, and `total_files` all `null`.

The behavior is correct: `ensure_with_progress_for_type` returns as soon as
`revision_is_ready` holds (`src/pullthrough.rs:111`), before any progress event is
emitted, so `record_progress` never runs. The record, however, is ambiguous. A job that
legitimately had nothing to transfer and a job that reached a terminal state having
moved zero bytes look identical to an operator reading the Admin API afterwards.

This was reported alongside the cold-miss failure of 2026-09-23 as an operability gap,
not as a defect.

## Write scope

- `src/admin.rs` terminal job record fields and their reconstruction;
- `docs/admin-api.md` job result documentation.

## Do not touch

- archive representation or publication semantics;
- job status vocabulary beyond what this distinction requires;
- making the index the authority for anything other than derived job history.

## Acceptance criteria

- A terminal prefetch record explicitly identifies the no-op case, so that "already
  archived, nothing to transfer" is machine-distinguishable from "completed having
  transferred zero bytes".
- The distinction survives index reconstruction from durable job state (ADR-0018) and
  does not require a new authoritative store.
- Existing recorded jobs remain readable; absence of the new information is not
  reported as a failure.
- `docs/admin-api.md` documents the field and its interpretation.

## Verification

```sh
nix develop -c cargo fmt --check
nix develop -c cargo clippy --all-targets --all-features -- -D warnings
nix develop -c cargo test --all-features
nix flake check
```

Add tests covering a prefetch of an already archived revision, a prefetch that
transfers files, and reconstruction of both records from durable state.

## Risks and assumptions

None material. The change is confined to reported job metadata; it must not alter when
a revision is considered ready or published.

## Implementation status

Implemented on 2026-09-24. Verified on x86_64-linux with `cargo fmt --check`,
`cargo clippy --all-targets --all-features -- -D warnings`, `cargo test --all-features`,
and `nix flake check`, each with its exit status taken directly rather than through a
pipe. An independent reviewer checked each acceptance criterion against the code and
confirmed that the load-bearing tests fail when the behavior they guard is broken.

The terminal job record carries `outcome`, which is `already_archived`, `published` or
`extended`. Records written before this change stay readable because the field defaults
when absent. Implemented together with Issue 0070 so the outcome vocabulary was designed
once, against the three results an acquisition can actually produce.

`nix flake check` omits aarch64-linux as an incompatible system, so the QNAP release
architecture is covered by the native GitHub Actions jobs, not by this run.
