# ADR-0021: Serialize transferring acquisitions

- Status: Accepted
- Date: 2026-09-24

## Context

ModelKeep delegates upstream transfer to the official Hugging Face client: one
acquisition is one `snapshot_download` invocation over a fixed selection, and that call
downloads the matching files with its own thread pool, eight by default, one thread per
file (ADR-0005, ADR-0003). The selection is fixed when the call is made. Verified
against the pinned `huggingface_hub` 1.27.0: there is no facility to add files to a
download already in progress, and no download queue or handle to hold. The `Job` and
`CommitScheduler` APIs the client exports are Hugging Face Jobs and uploads, unrelated
to downloading.

Because the single-flight key includes the selection, two requests for overlapping but
different selections of the same repository currently run as two concurrent
acquisitions. The overlapping files are transferred twice. On the deployment's ~6 Mbps
uplink that is not a rounding error.

The alternative to a policy here is for ModelKeep to own scheduling: keep a per-file
queue, batch its head into invocations, and let requests add to it. That is
implementable, and it would make request-level cancellation and overlap fall out as set
arithmetic. It also moves transfer scheduling, fairness, and batch sizing into ModelKeep
— work the official client is doing today — and it requires the upstream file list up
front plus a rework of fetch staging identity.

## Decision

At most one transferring acquisition runs per repository at a time. Further acquisitions
for the same repository wait and run one after another.

1. The scope is the repository, keyed by repository type and repository ID. Model and
   dataset repositories that share an ID are distinct (ADR-0019). Two revisions of one
   repository do not transfer concurrently.

2. Waiting is FIFO. A management job waits in its existing `queued` state, so it remains
   visible and cancellable while it waits. A client-driven acquisition waits as work, and
   the request that triggered it is answered under the cold-miss contract rather than
   held.

3. The gate covers transferring invocations only. Metadata and resolve-only invocations —
   including the reconciliation that computes what a published revision is still missing
   — are not gated, so an acquisition cannot block on a metadata call it makes itself.

4. Single-flight is unchanged and sits below this gate: identical work is still collapsed
   to one acquisition, and the gate serializes what remains distinct.

5. A configurable limit caps how many transferring acquisitions run at once across all
   repositories. The default is two. Acquisitions beyond the limit wait under the same
   FIFO and visibility rules as the per-repository gate.

6. ModelKeep does not implement per-file scheduling, batching, or fairness. Transfer
   scheduling stays with the official client, within one invocation.

## Rationale

Serializing costs latency for the second requester and saves the duplicate transfer
entirely, because by the time the second acquisition runs, the reconciliation added in
Issue 0070 compares its selection against what the archive now holds and transfers only
the remainder — which is often nothing. On a slow uplink, waiting is cheaper than
sending the same bytes twice.

It also keeps ModelKeep out of a scheduler it would have to get right. Fairness, batch
sizing, and per-file queue bookkeeping are real problems, and the official client already
solves the part that matters inside one invocation.

The global limit exists for two reasons that have nothing to do with the per-repository
rule. Concurrency buys no throughput on a saturated uplink: eight threads per acquisition
times N acquisitions is the same bytes per second, arriving later for everyone. And each
acquisition in flight holds its own partial data in staging, so N concurrent large
acquisitions multiply peak temporary capacity by N, on an archive where running out of
space is a failure class in its own right.

The default is two rather than one because a limit of one would queue a single small file
behind a multi-day transfer of an unrelated repository, and ModelKeep cannot currently
tell those apart before starting. Two bounds peak staging while leaving room for small
work to proceed alongside one large transfer. Size-aware ordering would need the upstream
file list and a scheduler, which this record deliberately does not adopt.

The cost this decision does accept is head-of-line blocking: a large acquisition delays
everything else for its repository. That is tolerable only because a running acquisition
can be cancelled (Issue 0076); without cancellation, one forgotten filter would block a
repository for days.

## Alternatives considered

- **Leave concurrent acquisitions as they are.** Rejected. Overlapping selections
  transfer the same bytes twice on a link where that is expensive.
- **Merge overlapping selections into one acquisition.** Rejected. Cancelling part of a
  merged acquisition has no clean meaning, and the merge has to happen before the
  transfer starts, which is exactly when the second request has not arrived yet.
- **A ModelKeep-owned per-file queue with batched invocations.** Deferred, not dismissed:
  it is the better long-term shape and makes request-level cancellation natural, but it
  moves scheduling into ModelKeep and depends on recording the upstream file list. If it
  is adopted, it supersedes this record.
- **Serialize per revision rather than per repository.** Rejected as insufficiently
  simple: it still allows two revisions of one repository to compete for the same uplink,
  and the operator reasons about repositories.

## Consequences

Acquisitions for one repository become predictable and sequential, and the archive stops
paying twice for overlapping requests. A second requester waits, and on the client side
that wait is already bounded by the cold-miss contract.

Head-of-line blocking is now possible per repository and is the reason cancellation is a
prerequisite rather than a nicety. The in-flight and queued views must make it obvious
what is blocking a repository.

Different repositories still transfer concurrently up to the global limit, so the uplink
is shared by a bounded number of acquisitions rather than by however many happen to be
requested. Raising the limit trades predictability and peak staging for the chance that
one stalled acquisition does not hold a slot; lowering it to one makes every acquisition
wait for every other.

Because the limit is small, a slot held by a long transfer is a scarce resource, which is
another reason cancellation and a clear view of what holds each slot are prerequisites
rather than conveniences.

## Validation

- A second acquisition for a repository does not start while one is transferring, and
  starts after it finishes.
- Two overlapping selections transfer each shared file once: the second acquisition's
  transferred bytes are measured, not asserted.
- A metadata or resolve-only invocation made by a running acquisition is not blocked by
  the gate.
- Acquisitions for different repositories run concurrently up to the limit, and the
  limit is honoured: with the limit set to one, a second repository's acquisition waits.
- The configured limit is reported at startup alongside the other effective settings.
- A queued management job waiting on the gate is visible and cancellable.
- Identical requests are still collapsed by single-flight rather than serialized.

## Measured throughput, and what it does to the estimates above

An acquisition on the deployment on 2026-09-24 moved 6,183,464,935 bytes in 818 seconds: an
average of **7.56 MB/s, about 60 Mbps**. The ~6 Mbps figure this record reasons from was
measured separately at 02:00 JST the same day, so the link is either variable or the two
measurements took different paths.

The wall-clock estimates derived from ~6 Mbps are therefore wrong for that acquisition by
roughly an order of magnitude: 469.92 GB is about 17 hours at 7.56 MB/s, not about seven days.

**The decisions here do not depend on those absolute figures.** What they rest on is the ratio
— 469.92 GB against 48.41 GB, 9.7x — and the ratio is a property of the repository, not of the
link. A filtered acquisition is worth the same multiple whatever the throughput. The absolute
numbers are corrected here rather than in place, so the reasoning as it was written stays
legible.
