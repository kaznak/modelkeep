---
status: open
priority: P1
related_adrs:
  - ADR-0008
  - ADR-0009
  - ADR-0010
  - ADR-0017
created: 2026-08-24
updated: 2026-09-23
---
# Issue 0056: Resume interrupted upstream acquisition safely

- Status: Open
- Priority: P1
- Related ADR: ADR-0008, ADR-0009, ADR-0010, ADR-0017

## Objective

Allow a new request or management job to resume reusable upstream download staging
after a container restart or recreation, without exposing or publishing incomplete
model data.

## Problem

An interrupted acquisition remains under `/data/tmp` until its staging lease expires,
but a retry allocates a new staging directory and downloads from the beginning. For a
large sharded model this can waste hours, temporarily double storage consumption, and
increase the chance of ENOSPC. The current behavior is data-safe because partial data
is never published, but it is operationally inefficient.

Management jobs that were active at restart should remain explicit interrupted
failures for auditability. Resumption belongs to a new request or job and must not
silently rewrite the history of the interrupted operation.

## Scope

- Persist enough credential-free acquisition identity to determine whether abandoned
  staging matches the repository, requested revision, resolved commit, and fetch
  selection of a new acquisition.
- Define an exclusive, crash-safe lease handoff from expired staging to one new
  operation; active or ambiguous staging must never be adopted.
- Delegate byte-level continuation to the supported official Hugging Face client only
  where its `local_dir` behavior is demonstrated to resume safely.
- Validate all resumed output through the existing complete-snapshot publication
  boundary before removing the lease and atomically publishing the revision.
- Remove or conservatively quarantine incompatible, corrupt, or unidentifiable
  staging without treating it as a cache hit or completed revision.
- Emit operational events that distinguish resumed, discarded, and newly started
  acquisition without logging credentials or signed URLs.

## Acceptance criteria

- Recreating the container during a fixture-backed multi-file acquisition leaves no
  partial revision observable through ModelKeep.
- A later retry adopts exactly one matching expired staging directory and transfers
  fewer upstream bytes than a complete restart of the same acquisition.
- Concurrent retries cannot adopt the same staging directory or perform duplicate
  publication.
- Active leases, mismatched repository/revision/commit/file selection, malformed
  metadata, and failed integrity checks are never resumed as trusted data.
- The resumed acquisition publishes only after the normal size, digest, manifest,
  flush, and atomic-rename checks succeed.
- The original management job remains `failed` with phase `interrupted`; the new job
  records whether it resumed prior staging.
- Restarting with upstream unavailable leaves the incomplete staging unpublished and
  returns an operationally meaningful failure rather than a false warm hit.

## Verification

```sh
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
nix flake check
```

Add black-box crash/recreation tests covering resumable success, incompatible
staging, concurrent adoption, integrity failure, upstream-offline retry, and ENOSPC
behavior. Run the native amd64 and arm64 GitHub Actions checks. Before closing this
issue, record one QNAP container recreation during a representative large prefetch
and confirm that transferred bytes and temporary capacity behave as designed.

Use the operator procedure in
[`qnap-resume-drill.md`](../deployment/qnap-resume-drill.md). Keep site endpoints,
operator identity, and raw job records in the ignored/private deployment record; add
only sanitized measurements and the outcome here.

## Risks and assumptions

The supported Hugging Face client may change its partial-download metadata or resume
behavior. ModelKeep must treat that state as an optimization, not durable archive
format or authority. If safe compatibility cannot be demonstrated for a client
version, starting a new acquisition is preferable to adopting ambiguous bytes.

## Implementation status

Implemented on 2026-09-23 with versioned fetch identity, commit-pinned helper reuse,
atomic expired-lease claiming, persisted `resumed` job state, and a black-box
SIGKILL/recreation test. The issue remains open until the required representative
QNAP recreation drill and both GitHub Actions architectures have completed.

The first QNAP drill confirmed that restart marks the original job interrupted and a
new job atomically adopts the retained staging with `resumed: true`. The resumed
official client then emitted a JSON diagnostic on stdout; the strict parser treated
that untyped object as a legacy result and rejected it as malformed. The helper now
reserves stdout for ModelKeep protocol events and redirects official client/transport
stdout to its discarded diagnostic stream. A regression test starts with retained
partial metadata and proves an untyped JSON diagnostic cannot enter the protocol
channel. Repeat the completion portion of the QNAP drill with an image containing
this fix before closing the issue.

The v0.4.6 QNAP repeat adopted approximately 12 GiB of retained data and again failed
with `MalformedResult` before transferring more bytes. This proved helper-side stdout
redirection alone was not a sufficient trust boundary: the Rust parser still treated
every untyped JSON object as a legacy result. The protocol now requires an explicit
`type: "result"`; untyped JSON diagnostics are ignored, while a helper that never
emits a typed result fails safely as `MissingResult`. The production fixture and
contract tests use typed results and preserve this observed resume-only regression.

The same drill also exposed that SIGTERM could leave the container waiting for an
active management prefetch. Management jobs used Tokio's blocking pool, whose runtime
shutdown waits indefinitely for blocking tasks. They now run on process-lifetime OS
threads so container exit interrupts the worker and leaves recovery to the durable job
record and staging lease. The black-box crash check submits a blocking Admin prefetch,
sends SIGTERM to PID 1, and requires a successful process exit within five seconds.

The completion portion of the drill finally passed on 2026-09-24 with v0.4.9. A
filtered prefetch of a commit-pinned four-file selection totalling 73,025,919,893
bytes, one file of which is 72 GB, was interrupted by a container force-stop during
transfer. It left 8,943,738,646 bytes of retained staging. A new job adopted that
staging, reported `resumed: true`, and ran to `outcome: published` with
73,025,919,893 of 73,025,919,893 bytes and four of four files.

So the resumed acquisition transferred 64,082,181,247 bytes where a complete restart
would have transferred 73,025,919,893 — 12.25 per cent fewer, matching the retained
amount. Temporary capacity behaved as designed: the archive filesystem never held a
second copy of the selection, and no partial revision was observable through
ModelKeep at any point.

One dependency remains before this issue closes. The retained staging was only
reachable after renaming its directory by hand, because the in-flight marker path
refuses a matching identity without checking whether the lease is still live. The
resume machinery was correct and unreachable. [Issue
0083](0083-release-expired-active-fetch-staging.md) owns that defect; this issue
closes when an interrupted acquisition of the same identity resumes with no manual
step.
