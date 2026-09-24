# ADR-0022: Record upstream file metadata, and answer repository metadata without acquiring

- Status: Accepted
- Date: 2026-09-24
- Refines: ADR-0020

## Context

Repository metadata for a revision the archive had never seen was answered by acquiring
the whole repository and reporting what the archive then held. A client reads metadata
before it requests any file, so its own `--include` was applied to a file list ModelKeep
had already paid for in full. Measured on 2026-09-24: a filtered download of an unseen
repository archived the complete snapshot.

ADR-0020 decided that the archive records what it holds and asserts nothing about upstream
completeness. Two reasons were given: such a claim is not reconstructible from archive
state, and no decision consumed it.

Both reasons have since changed.

There is now a consumer: answering metadata for files the archive does not hold requires
knowing that those files exist. And the fact required is not a claim about the archive but
a fact about the commit. `repo_info(..., files_metadata=True)` — which the helper already
calls while resolving a revision, keeping only the commit — returns, per file, the path,
the size, the git blob id, and the LFS sha256. A commit is immutable, so its file list is
permanently valid; it cannot go stale the way a claim about our own state could.

## Decision

1. When a revision is acquired, record the upstream per-file metadata for its commit: path,
   size, upstream git object id, and LFS sha256 where upstream reports one. The record
   covers the commit, not the selection that was acquired.

2. Store it in a file inside the revision directory whose name begins with `.modelkeep-`.
   `is_internal_archive_path` excludes such names, so the record appears in no manifest, no
   file listing and no response body, and an older binary does not see it at all. **The
   manifest format does not change.**

3. The record is a record of upstream, not a claim about the archive. Coverage may be
   *derived* by comparing it with the manifest when some decision needs it; it is never
   *asserted* as stored state. This refines ADR-0020 decision 2 rather than reversing it:
   what that record refused was an unverifiable and unconsumed claim about ModelKeep's own
   completeness. A file list is verifiable against upstream, and it has consumers.

4. Metadata for a revision the archive does not hold is answered from upstream, acquiring
   nothing and writing nothing to the archive. Upstream's response is not relayed: the
   answer is rebuilt from the file list, so nothing that points at an upstream payload path
   can pass through ModelKeep (core invariant 10).

5. Metadata for a revision the archive holds never contacts upstream. With a record it
   reports the union of the manifest and the record; without one it reports what the archive
   holds. Revisions that predate this decision are not migrated, and the absence of a record
   is a state to report, not a gap to fill with a substitute.

6. Reconciliation uses the record where it applies, removing the upstream round trip it
   previously required.

7. Metadata responses carry the Hub's fields with the Hub's meanings, and ModelKeep's own
   information beside them rather than inside them.

   A file's upstream git object id is reported under the Hub's name for it — `oid` in a
   tree entry, `blobId` in a `revision` sibling — or is absent when no record holds one. It
   is never substituted with a digest of ModelKeep's own.

   ModelKeep's digest is reported as `modelkeep: {"sha256": ...}` on every entry of both
   routes. It is the value the resolve route advertises as `ETag`, it is present for every
   file the archive holds, and it is `null` for a file the archive does not hold, because
   ModelKeep will not claim a digest for bytes it has not got. This is the field a client
   verifies a local copy against.

   `lfs` and `xetHash` are not emitted. Two measurements decide that, and the second
   corrects a claim an earlier draft of this record made:

   - An `lfs` object without `pointerSize` makes both pinned clients fail the whole
     download with `KeyError: 'pointerSize'`. ModelKeep does not record a pointer size, so
     emitting `lfs` faithfully would mean inventing one.
   - `lfs.oid` alone does **not** move the validator. This record previously said that
     `lfs` being present makes 1.27.0 skip its `HEAD`; that was too broad. The skip requires
     xet availability, a valid `xetHash`, an LFS sha256 and an LFS size together. With
     `lfs.oid` present and no `xetHash`, both versions still issue the `HEAD` and name the
     blob after the served `ETag`, and a deliberately divergent `lfs.oid` goes unused.

   Emitting a faithful `lfs` later requires recording a pointer size, which is an addition
   to decision 1 and a change to the helper.

8. A ref learned from an upstream metadata answer is held in memory only, and is used to
   create a ref that does not exist once the corresponding revision is published. No
   existing ref is ever moved, so ADR-0012 stands: normal requests still do not refresh an
   archived mutable ref. Losing the memo on restart costs a repeated metadata answer and
   nothing else.

## Rationale

Recording a fact about an immutable commit is a different act from asserting a property of
our own state, and only the second was ever the problem. The distinction matters because it
is what keeps ADR-0006 and core invariant 9 intact: the record is not authority over model
bytes, and nothing about the archive's own contents is known only from it.

Answering metadata without acquiring is what makes a client's own filter mean anything on a
first download, which is the whole point of the subset work in Issue 0070. Before this, that
work was reachable only through a filtered prefetch.

## Alternatives considered

- **Keep acquiring the repository to answer metadata.** Rejected: it makes a client-side
  filter cost the full repository, which for one measured case is 469.92 GB instead of
  48.41 GB.
- **Relay upstream's metadata response.** Rejected: it would pass through fields that point
  at upstream payload paths, which core invariant 10 forbids.
- **Record the file list in the manifest.** Rejected: it changes a durable format that older
  binaries read, for information that is not part of what the revision holds.
- **Report only the archived subset for a partially archived revision.** Rejected, and this
  is the substantive behaviour change: for a sharded model it hands back a model with shards
  missing and reports success. Failing on a file the archive does not hold is louder and
  therefore safer.
- **Derive coverage and store it.** Rejected for the reason ADR-0020 gave. Deriving it when
  needed costs nothing and cannot become stale or wrong.

## Consequences

A partially archived revision now reports files it does not hold. An unfiltered download of
such a revision therefore asks for them, which succeeds while upstream is reachable — adding
them to the same revision — and fails while it is not. That failure replaces a silent
success that returned an incomplete model.

The cost boundary for an unfiltered download is consequently the client's own filter plus
the ability to stop a transfer (Issue 0076). There is no size gate.

A chain in which one ModelKeep points at another propagates the record, because the
`revision` siblings now carry the Hub's per-file fields: the upstream git object id travels
as `blobId` and is recorded by the mirror downstream. That path is exercised end to end by
the supported-client checks.

Legacy and imported revisions keep working with no record and no migration; they gain one
the next time they are acquired.

## Validation

- A metadata request for an unarchived revision answers without invoking the fetcher.
- A real supported client filtering a download of an unseen repository archives only the
  matching files; measured for both pinned versions.
- Metadata for an archived revision answers with upstream unavailable.
- An unarchived revision with upstream unavailable fails meaningfully and publishes nothing.
- A partially archived revision reports files it does not hold, and requesting one extends
  the revision.
- A revision with no record reports what the archive holds.
- Reconciliation performs no upstream round trip where the record applies; the call count is
  measured.
- A ref learned from a metadata answer creates only a missing ref and never moves an
  existing one.
- `oid` and `blobId` carry upstream's git object id and are asserted *not* to equal
  ModelKeep's digest, so the two cannot quietly become the same field again.
- Every archived file carries `modelkeep.sha256` equal to the `ETag` served for it, and a
  file the archive does not hold carries `null` there while still reporting its upstream
  object id.
