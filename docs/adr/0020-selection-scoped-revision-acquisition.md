# ADR-0020: Selection-scoped revision acquisition

- Status: Accepted
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

First, the complete-snapshot invariant is asserted, not enforced. `write_manifest` emits
`"complete":true` unconditionally, and `import-hf-cache` publishes whatever a local
cache snapshot happens to contain. A cache produced by `hf download --include ...`
already becomes a revision recorded exactly like a whole-repository acquisition. The
archive can already hold partially covered revisions.

Second, the acquisition helper already supports selection. `hf_fetch.py` passes
`allow_patterns` to the official client and derives its expected file set from those
patterns, and the fetch staging identity already carries a file-selection component.
What is missing is archive semantics, not transfer capability.

Third, upstream completeness is not reconstructible from archive state. Reading every
byte the archive holds cannot reveal whether upstream has a file the archive never
recorded. Core invariant 9 requires metadata to be reconstructible from durable archive
state where practical, so a stored completeness claim would be exactly the kind of
unverifiable metadata this project avoids, and keeping such a claim true would require
comparing the archive against upstream.

## Decision

1. An acquisition may be restricted to a selection of include/exclude patterns.
   ModelKeep validates and normalizes patterns as untrusted input, rejecting traversal,
   absolute paths, and otherwise unsafe patterns, and delegates matching to the official
   client's `allow_patterns` / `ignore_patterns` rather than reimplementing it.

2. The archive records what it holds and asserts nothing about upstream completeness.
   No manifest field expresses coverage, and the manifest format does not change.
   `complete` keeps its existing meaning: every file listed in this manifest was fully
   acquired and verified before the manifest was published. A manifest is never
   published for a partially transferred set, so partial data remains unobservable
   exactly as under ADR-0008.

3. The serving rules are unchanged. A path listed in the live manifest is served warm
   and offline. A path that is not listed is an archive miss and follows the normal
   acquisition path; the archive never asserts on its own authority that such a path
   does not exist. Whether it exists is upstream's answer, obtained when the path is
   requested.

4. A revision's file set may grow; it may never change. An acquisition for a path an
   existing revision does not list extends that revision: it adds absent paths, never
   overwrites or removes a published path, and publishes by writing the new files
   durably and then atomically replacing the manifest with a superset. The existing
   publication path renames a staging directory onto `revisions/<commit>` and therefore
   cannot be reused for extension; extension is a separate, explicitly crash-safe
   operation. After a crash the live manifest is either the old one or the new one, and
   files on disk that the live manifest does not list are never served.

5. The normalized selection is part of acquisition identity: fetch staging identity,
   expired lease adoption under ADR-0017, and management-job idempotency all incorporate
   it. It is acquisition state, not a durable claim, and is not recorded in the
   published manifest.

   Adopting expired staging still requires the repository type, repository, requested
   revision, and resolved commit to match; that is not relaxed. On the selection,
   staging is adopted when its recorded selection is unrestricted, or identical to the
   request's. An unrestricted acquisition already covered every path a narrower request
   wants, so its retained bytes belong to the same paths of the same commit. The
   acquisition that adopts it stays restricted to the request's own selection, and
   publication lists only what the helper reports, so retained files outside that
   selection are never published as part of it. Two different restricted selections are
   not adopted for each other.

   Requiring the selections to be equal would discard the retained bytes of an
   interrupted repository-wide acquisition as soon as a client asked for one file of it,
   which is the operational loss ADR-0017 exists to prevent.

6. Whole-repository acquisition remains the default. A request that carries no selection
   behaves exactly as it does today.

## Rationale

The archive only ever has to answer two questions: whether it holds a requested path,
which the manifest answers exactly, and whether upstream has a path the archive does not
hold, which upstream answers when asked. "Does this revision hold every upstream file"
is not an input to any decision, so recording it would add an unverifiable claim that
nothing consumes and that could only be kept true by auditing against upstream.

What ADR-0008 actually protected is kept intact: a partially transferred set is still
never published, and a published manifest still means every file it lists is complete
and verified. What is dropped is the separate assertion that the list covers the whole
upstream repository, which was never established for imports and cannot be established
from archive state.

Monotonic growth keeps immutability where it matters. An upstream commit fixes the
content of every path it contains, so adding a path can never contradict a path already
published, and no published byte is ever rewritten. It also removes any need to migrate
existing revisions: a revision that does not hold a requested file is extended the first
time that file is requested, whatever produced it.

## Alternatives considered

- **Keep ADR-0008 unchanged.** Rejected. It blocks the primary production use, and it
  does not in fact prevent partially covered revisions, as the import path shows.
- **Record a coverage claim in the manifest, such as `snapshot` versus `partial`.**
  Rejected. It is not reconstructible from archive state, cannot be verified without
  contacting upstream, and no decision consumes it. Making it load-bearing — for
  example by refusing to extend a revision that claims to be a snapshot — would require
  an upstream audit to keep it true, and would turn a legacy partially covered revision
  into a request that transfers a whole repository and then fails at publication.
- **Mark partially covered revisions `complete: false`.** Rejected. The current code
  treats that value as corruption rather than as partial coverage:
  `repository_inventory_for_type` fails an entire repository listing on the first such
  revision. It would also force all four serving gates to be rewritten for no gain.
- **A separate revision directory per selection, keyed by a selection hash.** Rejected.
  It breaks the revision-equals-commit identity of ADR-0002, duplicates shared files,
  and multiplies ref semantics.
- **A mutable per-file state database.** Rejected under ADR-0006 and core invariant 9;
  metadata must not become the authority for model bytes.

## Consequences

The durable format does not change. Manifests keep their current fields, the four
serving gates keep their current meaning, and no archive written before this decision
needs migration or re-download.

A request for a path the archive does not hold always consults upstream. When upstream
is unavailable, that path fails with an upstream-unavailable error rather than a `404`.
Core invariant 8 is unaffected: every file the archive holds remains downloadable
without upstream access.

A client that downloads a partially archived revision receives the archived file set,
because repository metadata reports what the archive holds and never fabricates entries.
An operator who wants the whole repository requests it without a selection, and the
acquisition extends the same revision rather than creating a second one.

ADR-0008's rule that a published revision is necessarily a complete upstream snapshot no
longer holds. Partially covered revisions are ordinary, whether they came from a
filtered prefetch, a single-file pull-through, or an import.

The substantive implementation cost is the extension operation, which introduces the
first write into an already published revision directory.

## Validation

- Unit tests for selection normalization and unsafe-pattern rejection.
- Integration tests with supported real `hf` / `huggingface_hub` clients: filtered cold
  acquisition, warm and offline download of the archived subset, metadata listing the
  archived set, and a request for a path the archive does not hold.
- Extension tests: an extension adds files, never rewrites a published path, and leaves
  either the old or the new manifest live after an induced crash, with no unlisted file
  served.
- Resume tests: staging recorded under an unrestricted selection is adopted by a
  narrower request and restricts the acquisition to that request; two different
  restricted selections are not adopted for each other; a different repository,
  revision, or resolved commit is never adopted.
- A request for a file a previously imported revision does not hold extends that
  revision rather than re-acquiring the repository.
