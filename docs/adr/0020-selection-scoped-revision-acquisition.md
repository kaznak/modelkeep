# ADR-0020: Selection-scoped revision acquisition

- Status: Proposed
- Date: 2026-09-24
- Supersedes in part: ADR-0008

## Context

ADR-0008 decided that every pull-through acquisition downloads the complete upstream
snapshot, and deferred the per-file model until its completeness and crash-recovery
semantics could be specified separately. This record specifies them.

The deferred cost has become blocking in production. GGUF repositories ship every
quantisation in a single repository. `Qwen/Qwen3-Coder-Next-GGUF` is 469.92 GB, of
which one quantisation directory (48.41 GB) is wanted. On the deployment's measured
uplink of about 6 Mbps that is roughly seven days instead of roughly 18 hours, and it
multiplies peak staging capacity by nearly ten. Neither supported route can express the
subset: the management API takes no patterns, and the client-driven route discards the
requested file list before calling the helper.

Three facts constrain the decision.

First, the complete-snapshot invariant is asserted, not enforced. `write_manifest`
emits `"complete":true` unconditionally, and `import-hf-cache` publishes whatever a
local cache snapshot happens to contain, with no comparison against upstream. A cache
produced by `hf download --include ...` already becomes a revision that claims to be a
complete snapshot. The archive can already hold selection-scoped revisions; today they
are simply mislabeled.

Second, the acquisition helper already supports selection. `hf_fetch.py` passes
`allow_patterns` to the official client and derives its expected file set from those
patterns, and the fetch staging identity already carries a file-selection component.
What is missing is archive semantics, not transfer capability.

Third, `complete` is not used as a coverage flag anywhere in the current code. All four
readers treat it as "is this revision intact enough to serve", and
`repository_inventory_for_type` treats `complete: false` as corruption, failing the
entire repository listing rather than reporting a partially covered revision. Coverage
and publication integrity are therefore two different facts, and only one of them has a
field today.

## Decision

1. A revision manifest records its coverage explicitly. `coverage: "snapshot"` denotes
   a whole-repository acquisition. `coverage: "selection"` denotes an acquisition
   restricted to a recorded, normalized selection of include/exclude patterns, which is
   stored alongside it. A manifest with no `coverage` field is read as `"snapshot"`, so
   archives written before this decision keep their current meaning.

2. `complete` keeps its existing meaning and is not overloaded with coverage: every
   file listed in this manifest was fully acquired and verified before the manifest was
   published. A manifest is never published for a partially transferred set, so partial
   data remains unobservable exactly as under ADR-0008. A selection-scoped revision that
   finished acquiring its selection is `complete: true`.

3. The serving rules are unchanged. A revision is servable when `complete` is true. A
   path listed in the live manifest is served warm and offline; a path that is not
   listed is an archive miss and follows the normal acquisition path, never a `404`
   asserted by the archive.

4. Coverage is consulted by acquisition, not by serving. An acquisition for a path that
   an existing revision does not list is permitted to extend that revision when its
   coverage is `"selection"`, and must not be attempted as a re-publication. Extending a
   revision whose coverage is `"snapshot"` is refused, because such a revision asserts
   that it already holds every upstream file for the commit.

5. Repository metadata routes report the archived file set and identify a revision as
   selection-scoped. ModelKeep does not fabricate entries for files it does not hold, so
   an offline client sees exactly what it can download.

6. A revision's file set may grow; it may never change. An extension adds paths that are
   absent, must never overwrite or remove a published path, and publishes by writing the
   new files durably and then atomically replacing the manifest with a superset. The
   existing publication path renames a staging directory onto `revisions/<commit>` and
   therefore cannot be reused for extension; extension is a separate, explicitly
   crash-safe operation. After a crash the live manifest is either the old one or the
   new one, and files on disk that the live manifest does not list are never served.

7. The normalized selection is part of acquisition identity. Fetch staging identity,
   expired lease adoption under ADR-0017, and management-job idempotency all incorporate
   it. Staging recorded under a different selection is never adopted as resumable.

8. Pattern matching for acquisition is delegated to the official client's
   `allow_patterns` / `ignore_patterns`. ModelKeep validates and normalizes patterns as
   untrusted input, rejecting traversal, absolute paths, and otherwise unsafe patterns,
   and does not reimplement matching.

9. `import-hf-cache` records an imported snapshot with `coverage: "selection"` unless
   the imported file set is verified complete against upstream repository metadata for
   that commit. An import must not assert a coverage it did not check.

10. Whole-snapshot acquisition remains the default. A request that carries no selection
    behaves exactly as it does today.

## Rationale

Coverage and publication integrity are orthogonal, so they get separate fields. A
selection that has been fully acquired and verified is a healthy object, and saying so
with `complete: true` keeps the four existing serving gates unchanged; only the
acquisition path learns about coverage. Overloading `complete` would instead make every
healthy selection-scoped revision look like corruption to the current code, most
visibly in `repository_inventory_for_type`, which fails a whole repository listing on
the first `complete: false` revision.

Recording coverage also makes the archive honest about what it holds, which is the
property ADR-0008 actually wanted; emitting `"complete":true` for an unchecked file set
only resembles that property.

Monotonic growth keeps immutability where it matters. An upstream commit fixes the
content of every path it contains, so adding a path can never contradict a path already
published, and no published byte is ever rewritten.

## Alternatives considered

- **Keep ADR-0008 unchanged.** Rejected. It blocks the primary production use, and it
  does not in fact prevent partial revisions, as the import path shows.
- **Mark selection-scoped revisions `complete: false`.** Rejected. The current code
  treats that value as corruption rather than as partial coverage, so a healthy
  selection would break repository listings and return integrity errors instead of
  serving. It would also force all four serving gates to be rewritten for no gain. The
  argument that it protects an older binary reading a newer archive is weak: downgrade
  compatibility is not promised anywhere in this project, and the manifest `version`
  field is not even read back, so there is no existing mechanism that fences off an
  older reader.
- **Refuse to serve any selection-scoped revision.** Rejected. The archived subset would
  be unusable offline, which defeats the purpose and conflicts with core invariant 8.
- **A separate revision directory per selection, keyed by a selection hash.** Rejected.
  It breaks the revision-equals-commit identity of ADR-0002, duplicates shared files,
  and multiplies ref semantics.
- **A mutable per-file state database.** Rejected under ADR-0006 and core invariant 9;
  metadata must not become the authority for model bytes.

## Consequences

A client that downloads a selection-scoped revision receives the archived subset, and
repository metadata shows that the revision is selection-scoped. Because metadata
reports only the archived set, an unfiltered client download of such a revision
retrieves the subset rather than the whole repository.

A request for a path outside the recorded selection costs the acquisition of that path,
not of the repository, and extends the existing revision. Its response-time behavior is
governed separately by the cold-miss contract tracked in Issue 0069.

The serving gates do not change. The acquisition path gains coverage awareness and a new
crash-safe extension operation, which is the substantive implementation cost.

Manifests gain fields. Archives written before this decision are read unchanged.
Downgrade compatibility is not promised: an older binary ignores the new fields and
would serve a selection-scoped revision as though it were a snapshot, which is the
behavior it already exhibits for imported partial caches.

The import path becomes stricter: imports previously recorded as complete snapshots
without any check are recorded as selection-scoped. Revisions imported before this
change keep their recorded value; re-evaluating them is an explicit administrative
action, not an automatic rewrite.

## Validation

- Unit tests for selection normalization, unsafe-pattern rejection, manifest coverage
  round-tripping, legacy manifests with no `coverage` field, and refusal to extend a
  snapshot-coverage revision.
- Integration tests with supported real `hf` / `huggingface_hub` clients: filtered cold
  acquisition, warm and offline download of the subset, metadata listing the archived
  set, and a request for a path outside the selection.
- Extension tests: an extension adds files, never rewrites a published path, and leaves
  either the old or the new manifest live after an induced crash.
- Resume tests: staging recorded under a different selection is not adopted; staging
  under the same selection is.
- Import tests: an unverified partial cache is recorded as selection-scoped, not as a
  complete snapshot.
