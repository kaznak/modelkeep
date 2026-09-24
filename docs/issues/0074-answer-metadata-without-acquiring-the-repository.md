---
status: done
priority: P1
related_adrs:
  - ADR-0020
  - ADR-0005
  - ADR-0006
created: 2026-09-24
updated: 2026-09-24
---
# Issue 0074: Answer repository metadata without acquiring the repository

- Status: Done
- Priority: P1
- Related ADR: ADR-0020, ADR-0005

## Objective

Let a client's own file filter narrow a first acquisition, by answering repository
metadata for an unarchived revision without first acquiring the whole repository.

## Problem

A supported client reads repository metadata before it requests any file. For a
revision the archive has never seen, the metadata routes acquire the entire repository
and answer from the archive. The client's `--include` / `allow_patterns` is applied to
a file list that ModelKeep has already paid to acquire in full.

Measured with both pinned clients on 2026-09-24: a filtered download of an unseen
repository archives the full snapshot.

So the subset feature delivered by Issue 0070 is reachable only through a filtered
`prefetch` job, or against a revision that is already published, where a request for an
absent path extends it. The obvious client-side spelling of the same intent silently
costs the whole repository. For `Qwen/Qwen3-Coder-Next-GGUF` that is 469.92 GB instead
of 48.41 GB.

## Scope

Answer metadata for an unarchived revision from upstream repository information rather
than from an acquisition, and let the per-file requests that follow acquire only what
the client asks for.

Record the upstream file list for a commit when one is obtained. A commit is immutable,
so its file list is a fact that does not go stale, and recording it is not the kind of
unverifiable claim about our own state that ADR-0020 refused. A revision that knows its
upstream file list can report it while offline, so a partially archived revision stops
presenting its subset as the whole repository — which for a sharded model means handing
back a model with shards missing and calling it a successful download.

The cost is the mirror image: a client that downloads without a filter then asks for
every file the archive does not hold. That is acceptable only because Issue 0076 makes a
running acquisition cancellable, which turns a forgotten `--include` from days of
saturated uplink into the minutes before someone notices. **This issue depends on Issue
0076.**

### What upstream already gives us

Verified against the pinned `huggingface_hub` 1.27.0 on 2026-09-24. A single
`repo_info(..., files_metadata=True)` returns, per file:

| field | meaning |
| --- | --- |
| `rfilename` | the path |
| `size` | the byte length |
| `blob_id` | the git blob hash — **what upstream serves as `ETag` for a non-LFS file** |
| `lfs.sha256` | the LFS object digest — **what upstream serves as `x-linked-etag`** |

`hf_fetch.py` already makes exactly that call while resolving a revision, and discards
everything but the commit. So the upstream file list and both kinds of upstream validator
are available from a call ModelKeep already pays for.

That makes this issue and Issue 0078 one mechanism rather than two. Recording this
per-file metadata gives Issue 0074 the file list it needs to answer metadata without
acquiring, and gives Issue 0078 the faithful validator to serve — the same value the real
Hub would return for that file, chosen by the same rule. It also removes the upstream
round trip from Issue 0070's reconciliation, which becomes a local set operation, and it
is the prerequisite for letting the official client skip already-archived files by its own
bookkeeping instead of ModelKeep subtracting sets at all.

Note that `snapshot_download(dry_run=True)` is **not** a substitute: its `DryRunFileInfo`
carries `commit_hash`, `file_size`, `filename`, `is_cached` and `will_download`, and no
validator of any kind.

Points to settle before implementing:

- What metadata is served when upstream is unavailable and the revision is unarchived.
  An archived revision must keep answering from the archive (core invariant 8).
- Whether a metadata answer that was not derived from archived state is distinguishable
  to an operator, and whether it is cached at all.
- That this stays metadata only. Payload must never be served from, or redirected to,
  upstream (core invariant 10).
- How it interacts with the metadata cold-miss policy, which currently waits for the
  acquisition by default because the supported clients retry nothing on these routes.

## Acceptance criteria

- A filtered download of an unseen repository through a real supported client archives
  only the matching files.
- An archived revision answers metadata without contacting upstream.
- An unarchived revision with upstream unavailable fails in an operationally meaningful
  way and does not publish or serve fabricated metadata.
- Payload acquisition still goes through ModelKeep; no redirect to upstream or Xet.

## Verification

```sh
nix develop -c cargo fmt --check
nix develop -c cargo clippy --all-targets --all-features -- -D warnings
nix develop -c cargo test --all-features
nix flake check
```

Extend the supported-client integration checks with a filtered download of an unseen
repository, asserting the archived set is the filtered set.

## Risks and assumptions

Serving metadata that is not backed by archived state is a change to what a metadata
answer means, and it must not become a path by which ModelKeep reports files it cannot
deliver. If that cannot be made safe, the alternative is to keep the current behavior
and document the filtered prefetch as the only supported way to archive a subset.

## Implementation status

Implemented on 2026-09-24 and recorded as ADR-0022. Verified on x86_64-linux with
`cargo fmt --check`, `cargo clippy --all-targets --all-features -- -D warnings`,
`cargo test --all-features`, and `nix flake check`, each with its exit status taken
directly.

Both pinned clients filtering a download of an unseen repository now archive only the
matching file, where before they archived all seven. Reconciliation's upstream round trips
went from two to zero where the record applies, measured by call count.

The upstream per-file metadata is recorded in a `.modelkeep-` prefixed file inside the
revision, so it reaches no manifest, listing or response, and an older binary does not see
it. The manifest format is unchanged and nothing is migrated: a revision with no record
reports what the archive holds.

Two findings from the implementation are worth keeping:

Answering metadata without acquiring stopped ModelKeep learning a ref, so `main` returned
`404` when ModelKeep was itself used as an upstream — caught by the 1.27 client check, not by
a unit test. An upstream-answered ref is now memoised in memory and used only to create a ref
that does not exist, so ADR-0012 stands.

With `lfs` present in a tree entry, 1.27.0 skips its `HEAD` and takes `lfs.oid` as the
validator, moving it off the value ModelKeep computes and serves. ADR-0022 decision 7
therefore omits `lfs` for now, and Issue 0079 has to resolve that rather than work around it.

Two existing assertions changed. They pinned a partially archived revision reporting only its
subset and an unfiltered offline download succeeding, which is the behaviour this issue
deliberately ends. The byte-level assertion moved to the filtered download and the unfiltered
case now asserts failure, so the replacement is stronger rather than weaker.

**Remaining**: `repository_info` siblings still carry only `rfilename`, so a chain of
ModelKeep instances does not propagate the record. Issue 0079 covers the reporting surface.
