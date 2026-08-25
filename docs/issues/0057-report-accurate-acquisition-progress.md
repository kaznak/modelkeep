---
status: open
priority: P1
related_adrs:
  - ADR-0005
  - ADR-0015
created: 2026-08-25
updated: 2026-08-25
---
# Issue 0057: Report accurate revision-level acquisition progress

- Status: Open
- Priority: P1
- Related ADR: ADR-0005, ADR-0015

## Objective

Give operators truthful, actionable progress for a complete revision acquisition,
including very large, concurrently downloaded Hugging Face/Xet-backed repositories.

## Problem

The upstream helper forwards progress from each `tqdm` instance as an unscoped
`completed` and optional `total` value. `JobManager::record_progress` then retains the
maximum values seen across all events. Those values are not necessarily cumulative
revision-level bytes: different files, parallel transfers, or transport phases may
create independent progress bars with different totals.

Consequently, the management UI can appear stuck at the size reached by one progress
bar while other files continue downloading. Its displayed maximum can also increase
late in the job as a larger progress bar is discovered. The resulting percentage and
transfer rate can be misleading enough that an operator may interrupt a healthy
multi-hundred-gigabyte acquisition. This was observed during a roughly 1.5 TB
`moonshotai/Kimi-K3` prefetch, where the displayed value remained near 407 GiB and
the apparent maximum subsequently increased even though network and disk activity
continued.

After the downloader returns, file discovery, validation, flush, manifest creation,
and atomic publication can also take material time for a large revision. The job
currently provides insufficient phase detail to distinguish that work from a stalled
download. Although job records expose creation and update timestamps, the UI does not
show when execution actually began or how long the acquisition has been running.
File progress is also often absent while an Xet-backed acquisition is active and
appears only after download completion, because the current helper reports file
counts only when the underlying client happens to emit a non-byte `tqdm` event.

## Scope

- Define a versioned progress contract between the official Hugging Face helper and
  ModelKeep that distinguishes revision-level totals from individual concurrent
  transfer progress.
- Aggregate parallel file/shard progress without double counting, regressing, or
  treating the largest individual progress bar as the revision total.
- Determine the number of archive-eligible files for the pinned revision before
  payload transfer where supported, and report completed-file progress independently
  of byte progress throughout the download.
- Report total bytes only when the value has revision-level meaning. Display an
  explicitly unknown total instead of manufacturing a percentage when the supported
  client or transport cannot provide one reliably.
- Preserve useful liveness information independently of byte growth, including the
  time of the last valid progress event.
- Persist and expose the time a queued job actually enters the running state,
  separately from its creation time, and show the start time and elapsed duration in
  the management UI.
- Expose post-download phases such as inventory, validation, durability sync, and
  publication so a long finalization step is not presented as a stalled download.
- Keep progress operational and reconstructible; it must not become authority for
  archive completeness or bypass the existing publication and integrity checks.

## Acceptance criteria

- A fixture with multiple concurrent files of different sizes reports cumulative
  revision bytes exactly once per byte and never substitutes the largest file's
  progress for the revision total.
- Displayed completed bytes are monotonic. When a total is known, completed bytes do
  not exceed it and the total does not grow merely because another per-file progress
  bar appears.
- When no trustworthy revision total is available, the API and UI say that the total
  is unknown and do not show a determinate percentage or misleading estimated
  completion.
- Transfer rate is calculated from revision-level byte deltas only; liveness remains
  visible when a valid event contains no increase in cumulative bytes.
- Active acquisitions display `n / m files` from the time the target file set is
  known. A file counts as complete only once its materialized output is complete; a
  retry, reset progress bar, or concurrent worker must not count it twice. Helper
  metadata under `.cache` is not included in either number.
- The API distinguishes job creation time from execution start time. The UI displays
  an unambiguous start date/time in the browser's local timezone and a live elapsed
  duration for running jobs; completed, failed, and cancelled jobs retain their
  recorded timing without deriving it from the latest page load.
- The job reports distinguishable downloading, inventory/validation, syncing, and
  publication phases through completion or an operationally meaningful failure.
- Restarted UI polling and persisted job records retain consistent progress semantics
  without changing archived revisions, manifests, refs, or model files.
- Observations against every supported real `huggingface_hub`/`hf_xet` combination
  document which progress events are available and preserve the behavior in a
  regression test.

## Verification

```sh
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
nix flake check
```

Add helper contract and management API/UI tests for sequential files, parallel files,
late or missing totals, repeated/reset per-file counters, liveness-only events,
file completion before terminal download return, helper-metadata exclusion,
creation-versus-start timing, elapsed-time rendering, and post-download phases. Run
the supported real Hugging Face client integration suite.
Before closing the issue, record one QNAP prefetch of a representative multi-shard
model and compare UI/API progress with network traffic, staging growth, and the final
manifest logical byte total.

## Risks and assumptions

The official Hugging Face client and `hf_xet` may change their progress-bar structure
between supported versions. ModelKeep must rely on an evidence-backed adapter or its
own stable aggregation identifiers rather than undocumented `tqdm` construction
details. Pre-enumerating file sizes may add an upstream metadata request or latency;
if a reliable total cannot be obtained without weakening compatibility, truthful
unknown-total progress is acceptable.
