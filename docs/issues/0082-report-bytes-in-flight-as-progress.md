---
status: open
priority: P2
related_adrs:
  - ADR-0018
created: 2026-09-24
updated: 2026-09-24
---
# Issue 0082: Find out why progress under-reports on one transfer path and not the other

- Status: Open
- Priority: P2
- Related ADR: ADR-0018

## Objective

Determine why a running acquisition's reported progress tracked the bytes in flight exactly on
one measured transfer and under-reported them badly on another, then make the reporting
trustworthy on both paths, so an operator can tell a slow transfer from a stalled one.

## What was measured

Two acquisitions on the deployment on 2026-09-24, both v0.4.9, both through the same helper
and the same `ProgressReporter`.

### Measurement A — progress under-reported

`Qwen/Qwen2.5-3B-Instruct`, commit-pinned, twelve files, 6,183,464,935 bytes, ten of them
small and two large shards. Job `started_at=1790226437`, `finished_at=1790227255`.

```text
state=running files=10/12 progress_bytes=36667596   fs_consumed=2717982720
state=running files=10/12 progress_bytes=36667596   fs_consumed=2718007296
state=running files=10/12 progress_bytes=36667596   fs_consumed=2718003200
state=running files=10/12 progress_bytes=36667596   fs_consumed=2718027776
state=running files=10/12 progress_bytes=56683812   fs_consumed=2738020352
```

`fs_consumed` is the drop in the archive filesystem's available bytes since the watcher
started. About 2.7 GB of the job's own data was on disk — consistent with 300 s at the job's
eventual 7.56 MB/s average — while the reported figure was 36.7 MB.

Note what the samples do and do not show. The **offset** of roughly 2.68 GB is the defect.
The **deltas** track: +20,016,216 reported against +19,992,576 on disk. So bytes arriving
during the sampling window were counted; roughly 2.68 GB that had arrived before it were not.

The job completed with `progress_bytes` exactly equal to `total_bytes`.

### Measurement B — progress tracked to the byte

`GZGavinZhao/Leanstral-1.5-119B-A6B-GGUF@929d4958b79a5625d189addd7c25be362748aed4`, four
files, 73,025,919,893 bytes, one of them 72 GB and in flight throughout. Six samples 30 s
apart:

```text
state=running files=3/4 progress=23196522014/73025919893  d_progress=0          d_fs=0
state=running files=3/4 progress=23196522014/73025919893  d_progress=0          d_fs=-4096
state=running files=3/4 progress=23263564994/73025919893  d_progress=67042980   d_fs=67092480
state=running files=3/4 progress=23466671944/73025919893  d_progress=203106950  d_fs=203026432
state=running files=3/4 progress=23466671944/73025919893  d_progress=0          d_fs=53248
state=running files=3/4 progress=23734837933/73025919893  d_progress=268165989  d_fs=268210176
```

`files` never advanced past 3/4 and the reported figure still reached 23.7 GB, so the bytes of
the single file in flight were counted. Every delta matches filesystem growth to within
filesystem overhead, and the two flat samples are flat on both sides — the transfer genuinely
paused, and the reporting said so.

## Correction to the original report

This issue was first filed asserting that bytes arriving in a live transfer are never counted
and that an operator was shown "two per cent of the truth". Measurement B refutes the general
claim, and re-reading Measurement A refutes the reasoning: the deltas there tracked, so the
counting mechanism was working and the fault is a missing 2.68 GB, not an absent mechanism.
The original text also treated `fs_consumed` as bytes written during the sampling window when
the watcher's baseline was taken at an arbitrary moment after the job started.

## What distinguishes the two

The transfer path, on the available evidence.

Measurement A ran on the container instance that preceded the Issue 0086 fix. Before that fix
`HF_HOME` pointed into a root-owned 0755 tmpfs while the process runs as 10001, and every
Xet-backed transfer failed with `EACCES`. Measurement A completed, so it did not go through
Xet. Measurement B ran after the fix, on a server whose startup self-check completed at
`1790233158` — 6,721 s after Measurement A finished — with `HF_XET_CACHE` on the mounted
volume, and Xet working.

So the working hypothesis is that the plain HTTP path stages partial data somewhere the
reporter does not see, while the Xet path stages it where it does.

**The mechanism is not established.** Two candidates were checked and neither explains
Measurement A:

- The reporter globs `*.incomplete` non-recursively under
  `<output>/.cache/huggingface/download`. The client's `incomplete_path` builds
  `metadata_path.parent / "{short_hash}.{etag}.incomplete"`, and `metadata_path.parent`
  mirrors the repository's subdirectory structure, so files in subdirectories are indeed
  missed. But `Qwen/Qwen2.5-3B-Instruct`'s shards are at the repository root, where the glob
  does match.
- The helper passes `local_dir`, so the local-folder layout applies in both measurements. The
  blob-cache layout, whose incomplete files live somewhere else entirely, is not in play.

## Scope

1. Establish the mechanism before changing the reporter. Reproduce Measurement A's shape with
   Xet disabled for the helper process and inspect the staging layout on disk while the
   transfer runs. Record the finding as an observation under `docs/observations/`, keeping the
   upstream client behaviour separate from ModelKeep's policy.
2. Count the bytes of files in flight on both paths.
3. Fix the non-recursive glob regardless of whether it explains Measurement A. A repository
   whose large files live in subdirectories is under-reported today, and that is a defect
   reachable by inspection.
4. Keep the guarantee that a constant byte count is not presented as fresh progress. Both
   flat samples in Measurement B were genuinely flat; that must stay visible rather than be
   smoothed over.
5. Leave the terminal value alone; it is correct in both measurements.

## Acceptance criteria

- The mechanism behind Measurement A is named in an observation record, with the on-disk
  staging layout that produced it.
- For an acquisition whose selection is dominated by a few large files, reported progress
  tracks bytes arriving rather than file completions, on both transfer paths. A test drives a
  fixture whose files are large relative to the reporting interval and asserts the reported
  figure advances between completions.
- A test covers a selection whose large file is in a subdirectory, and asserts its in-flight
  bytes are counted. This one fails today.
- The reported figure never exceeds the total, and still equals the total at completion.
- A genuinely stalled transfer is still distinguishable from a progressing one.
- Tests assert the relationship against a known transferred amount rather than asserting that
  some event was emitted.

## Verification

```sh
nix develop -c cargo fmt --check
nix develop -c cargo clippy --all-targets --all-features -- -D warnings
nix develop -c cargo test --all-features
nix flake check
```

For the protocol-facing part, reproduce both shapes against a real client: one acquisition
with Xet available and one without, each with a file large enough to remain in flight across
several progress events, comparing the reported bytes against the staging directory's actual
size rather than against the filesystem's free space.

## Risks and assumptions

The comparison against `archive_filesystem_available_bytes` is a shared-volume measurement and
cannot isolate one job's writes; it was adequate for deltas and misleading for absolute
offsets, which is how the original report went wrong. Reproduction should measure the staging
directory directly.

Priority is P2 rather than P1 because the path in use after the Issue 0086 fix reports
correctly; the degraded path is the fallback one. The subdirectory defect is real on both.
