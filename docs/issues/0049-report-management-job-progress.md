---
status: open
priority: P1
related_adrs:
  - ADR-0015
created: 2026-08-24
updated: 2026-08-24
---
# Issue 0049: Report progress for long-running management jobs

- Status: Open
- Priority: P1
- Related ADR: ADR-0015

## Objective

Make a multi-gigabyte prefetch visibly active and diagnosable through the management
API, structured logs, and Web UI while acquisition is in progress.

## Problem

Management jobs currently expose `progress_bytes` and `total_bytes`, but prefetch and
refresh leave both values unset until the operation completes. The UI therefore shows
only `acquiring_snapshot · total unknown`, and the server emits no periodic evidence
that a large Hugging Face download is still advancing. On QNAP this makes a healthy
large-model transfer indistinguishable from a stalled helper, unavailable upstream,
or storage I/O problem.

## Scope

- Extend the official Hugging Face helper protocol with structured progress events;
  do not scrape human-oriented progress bars or depend on private client internals.
- Track useful progress for prefetch and refresh jobs, including the current phase,
  completed and total bytes when known, completed and total files when known, the
  current file when available, and the timestamp of the last forward progress.
- Persist throttled job snapshots so API polling and process diagnostics see recent
  state without forcing an archive-disk write for every downloaded chunk.
- Emit credential-safe structured lifecycle and periodic progress events at a bounded
  cadence, including job ID and repository but excluding tokens and signed URLs.
- Update the management UI automatically while a job is queued or running and show a
  progress bar or an explicit indeterminate state, byte and file counts, current
  activity, transfer rate over a documented window, and time since last progress.
- Warn when a running job has made no observable progress for a documented interval;
  a warning must not itself mark the job failed or publish partial archive content.
- Preserve the existing atomic staging and publication semantics. Progress is
  operational metadata and must never imply that a revision is complete.

## Acceptance criteria

- During a large prefetch, repeated `GET /api/admin/v1/jobs/{id}` responses change as
  bytes or files advance and expose a recent last-progress timestamp.
- Known totals produce bounded progress (`0 <= completed <= total`); unknown totals
  remain explicitly indeterminate rather than reporting a misleading percentage.
- The Web UI refreshes running jobs without a manual page reload and visibly
  distinguishes advancing, indeterminate, stalled, completed, and failed states.
- Container logs provide bounded periodic evidence of forward progress and a final
  completion or failure event without leaking credentials or excessive per-chunk
  output.
- A helper or server restart leaves the job in the existing explicit interrupted
  state and never exposes staging data as a completed revision.
- Progress-reporting failures do not corrupt or prematurely publish model data.
- Automated tests cover event parsing, monotonic counters, throttling, unknown totals,
  stalled display state, interruption, and credential-safe logging.

## Verification

```sh
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
nix flake check
```

In addition, start a prefetch of a repository containing at least one multi-gigabyte
file, poll its job endpoint, observe the management UI and container logs during the
transfer, and verify the completed revision after publication. Repeat with upstream
traffic interrupted long enough to exercise the stalled indication.

## Risks and assumptions

The official `huggingface_hub` client may know file totals before it can provide a
reliable byte total, so the API and UI must support partial and unknown totals.
Progress callbacks cross the Python-helper/Rust-process boundary and require a framed
machine-readable stream that cannot be confused with diagnostics. Persisting every
callback would amplify writes on the durable QNAP volume, so updates and logs must be
rate-limited without hiding meaningful forward progress.
