---
status: open
priority: P1
related_adrs:
  - ADR-0018
created: 2026-09-24
updated: 2026-09-24
---
# Issue 0082: Report bytes in flight as progress

- Status: Open
- Priority: P1
- Related ADR: ADR-0018

## Objective

Make a running acquisition's reported progress reflect the bytes actually moving, so an
operator can tell a slow transfer from a stalled one.

## Problem

Measured on the deployment on 2026-09-24 with v0.4.9. A commit-pinned prefetch of
`Qwen/Qwen2.5-3B-Instruct` — twelve files, 6,183,464,935 bytes, ten of them small and two
large shards — reported this five minutes in:

```text
state=running files=10/12 progress_bytes=36667596   fs_consumed=2717982720
state=running files=10/12 progress_bytes=36667596   fs_consumed=2718007296
state=running files=10/12 progress_bytes=36667596   fs_consumed=2718003200
state=running files=10/12 progress_bytes=36667596   fs_consumed=2718027776
state=running files=10/12 progress_bytes=56683812   fs_consumed=2738020352
```

`fs_consumed` is the drop in the archive filesystem's available bytes since the job started.
So roughly 2.7 GB had been written while `progress_bytes` reported 36.7 MB — about two per
cent of it.

The job went on to complete: 6,183,464,935 bytes in 818 seconds, an average of 7.56 MB/s,
and `progress_bytes` finished exactly equal to `total_bytes`. So the number is right at the
end and wrong throughout.

The shape explains it. Ten small files finished early and account for the 36.7 MB. The two
large shards were in flight and contributed nothing until they completed. Issue 0068's work
made progress include retained `.incomplete` bytes when an acquisition *resumes*; bytes
arriving in a live transfer are not counted the same way.

## Why this matters more than it looks

Issue 0068 stated the requirement this violates: progress tests must distinguish byte
movement from completed files, and *a constant byte count cannot be presented as fresh
transfer progress*. That is exactly what an operator saw here for minutes at a time, and for
a repository with few large files it would hold for hours.

It also undercuts what Issue 0076 was built for. Cancelling a running acquisition is an
operator judgement — is this worth the link time it is consuming — and the number offered to
support that judgement was showing two per cent of the truth. An operator deciding from it
would conclude a healthy transfer had stalled.

## Scope

- Count the bytes of files in flight, not only files completed, in what a running job reports.
- Keep the guarantee that a constant byte count is not presented as fresh progress: if bytes
  genuinely are not moving, that must remain visible as such rather than hidden behind a
  number that only updates on file completion.
- Leave the terminal value alone; it is already correct.

## Acceptance criteria

- For an acquisition whose selection is dominated by a few large files, reported progress
  tracks bytes arriving, not file completions. A test drives a fixture whose files are large
  relative to the reporting interval and asserts the reported figure advances between
  completions.
- The reported figure never exceeds the total, and still equals the total at completion.
- A genuinely stalled transfer is still distinguishable from a progressing one.
- A test asserts the relationship against a known transferred amount rather than asserting
  that some event was emitted.

## Verification

```sh
nix develop -c cargo fmt --check
nix develop -c cargo clippy --all-targets --all-features -- -D warnings
nix develop -c cargo test --all-features
nix flake check
```

Reproduce the shape that exposed it: a fixture with several small files and at least one file
large enough to remain in flight across multiple progress events, asserting the reported bytes
advance while no file completes.

## Risks and assumptions

The bytes in flight have to come from somewhere the helper can report without depending on
private client internals; the existing progress channel already carries the official client's
own reporting, so the question is what ModelKeep does with it rather than whether the
information exists. Counting a partial file's bytes must not let the reported total exceed the
real one, and must not resurrect the defect where an unchanged counter looked like movement.
