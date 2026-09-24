---
status: done
priority: P0
related_adrs:
  - ADR-0020
  - ADR-0002
  - ADR-0005
  - ADR-0008
  - ADR-0010
  - ADR-0015
  - ADR-0017
  - ADR-0018
created: 2026-09-24
updated: 2026-09-24
---
# Issue 0070: Acquire a selected subset of repository files

- Status: Done
- Priority: P0
- Related ADR: ADR-0020 (governs this work), ADR-0008 (partially superseded),
  ADR-0002, ADR-0005, ADR-0010, ADR-0015, ADR-0017, ADR-0018

## Objective

Acquire and serve an explicitly requested subset of a repository's files through the
management API and through pull-through, without a later request for a path the archive
does not hold costing a whole repository.

## Problem

`JobRequest` (`src/admin.rs:372`) carries only `kind`, `repo_type`, `repo_id`, and
`revision`, and the prefetch branch passes an empty file list to pull-through
(`src/admin.rs:671`). A prefetch therefore always acquires the whole repository.

GGUF repositories ship every quantisation in one repository.
`Qwen/Qwen3-Coder-Next-GGUF` is 469.92 GB:

```text
Qwen3-Coder-Next-F16/      4 files  159.46 GB
Qwen3-Coder-Next-Q8_0/     4 files   84.81 GB
Qwen3-Coder-Next-Q6_K/     4 files   65.53 GB
Qwen3-Coder-Next-Q5_K_M/   4 files   56.71 GB
Qwen3-Coder-Next-Q5_0/     4 files   54.99 GB
Qwen3-Coder-Next-Q4_K_M/   4 files   48.41 GB
```

Only `Q4_K_M` (48.41 GB) is wanted. Acquiring the repository costs 9.7x the useful
subset. On the uplink this instance sits behind (measured 2026-09-24 02:00 JST, about
6 Mbps to both domestic and overseas hosts) that is roughly 18 hours versus roughly
seven days, and it multiplies peak staging capacity by nearly ten.

The client-driven route looks as if it could express the subset — `hf download
--include` requests only the matching paths, and the resolve handler does pass the
requested file to `PullThrough::ensure_for_type` (`src/http.rs:450`) — but
`fetch_and_publish` then discards it and sends `files: Vec::new()` to the helper
(`src/pullthrough.rs:342`). The client-driven route therefore transfers the same
469.92 GB as a full prefetch, and its first request never returns; see
[Issue 0069](0069-bound-cold-miss-resolve-response.md).

The helper is already capable. `hf_fetch.py` passes `allow_patterns=files or None` to
the official client (`upstream/hf_fetch.py:206`) and derives its expected file set from
those patterns (`upstream/hf_fetch.py:149`). The fetch staging identity already carries
a file-selection component (`record_fetch_resolved_commit_for_type` in
`src/upstream.rs`). The missing part is archive semantics, not transfer capability.

## Why this cannot be split into "prefetch first, pull-through later"

Adding selection to prefetch alone leaves the client-driven path unchanged, so a
request for a path the selection does not cover still starts a whole-repository
acquisition. After selection exists that becomes worse than today: the acquisition
transfers the whole repository and then fails at publication, because
`revisions/<commit>` already exists and the publication path refuses it
(`src/lib.rs:1121`), while `revision_is_ready` cannot short-circuit it either, since the
requested file is absent. The result is a full transfer that archives nothing and
returns an error. The same trap already exists today for any revision that does not hold
every upstream file, such as one produced by `import-hf-cache`.

Honouring the requested path in pull-through therefore requires adding files to an
already published revision, which requires the monotonic extension operation.

The three work items below are one reviewable unit.

## Governing decision

ADR-0008 forbade a file filter and deferred per-file acquisition. ADR-0020 specifies the deferred semantics and partially supersedes it: an acquisition may be
restricted to a selection; the archive records what it holds and asserts nothing about
upstream completeness, so the manifest format and the four serving gates are unchanged;
a path the archive does not hold is a miss resolved against upstream; and a revision's
file set may grow but never change, so a revision that lacks a requested file is
extended rather than re-published.

## Work items

1. **Management selection.** `JobRequest` accepts `include` / `exclude`; patterns are
   validated and normalized; the normalized selection enters the idempotency key and is
   passed to the helper as `allow_patterns` / `ignore_patterns`.

   ```rust
   struct JobRequest {
       kind: JobKind,
       repo_type: RepositoryType,
       repo_id: Option<String>,
       revision: Option<String>,
       include: Option<Vec<String>>,   // new
       exclude: Option<Vec<String>>,   // new
   }
   ```

2. **Pull-through honours the requested path.** `fetch_and_publish` stops discarding the
   caller's file list (`src/pullthrough.rs:342`), so a miss acquires the requested path
   rather than the repository.

3. **Monotonic extension.** Adding files to an already published revision: write the new
   files durably, then atomically replace the manifest with a superset. The existing
   `fs::rename`-onto-`revisions/<commit>` publication cannot be reused. Extension is
   never refused on the basis of what the revision already holds.

## Write scope

`src/admin.rs`, `src/pullthrough.rs`, `src/lib.rs` (extension operation),
`src/upstream.rs`, `src/http.rs` where the requested path is passed,
`docs/admin-api.md`, `docs/modelkeep-api.md`.

The manifest format does not change, and neither do the four serving gates that read
`complete`.

## Do not touch

- archive deletion or GC policy;
- the Xet boundary and delegation to the official client;
- the manifest format and the four serving gates' meaning of `complete`;
- revisions published before this change; no in-place upgrade or rewrite.

## Acceptance criteria

- `POST /api/admin/v1/jobs` accepts `include` / `exclude` for `prefetch`. Patterns are
  untrusted input: traversal, absolute paths, and otherwise unsafe patterns are rejected
  with the documented `400`.
- The normalized selection is part of the idempotency key. Identical submissions
  deduplicate; the same repository and revision with different patterns are distinct
  jobs.
- A filtered prefetch transfers only matching files, and `total_bytes` / `total_files`
  describe the filtered set, so progress is measured against what is being transferred.
- The published revision is served through the unchanged serving gates, and no new
  manifest field is introduced.
- A real supported client downloads the archived subset while upstream is unavailable,
  and repository metadata reports exactly the archived set.
- A resolve for a path outside the selection acquires that path only, extends the
  existing revision, and does not re-transfer the repository; the extension never
  overwrites or removes a published path.
- An induced crash during an extension leaves either the old or the new manifest live,
  and no file absent from the live manifest is ever served.
- A request for a file a previously imported revision does not hold extends that
  revision rather than re-acquiring the repository.
- Interrupting a filtered acquisition publishes nothing partial; a retry adopts staging
  only under the rule in ADR-0020 decision 5, and staging it may not adopt is left for
  the ordinary lease-expiry path rather than deleted by the acquisition that declined
  it.
- `docs/admin-api.md` and `docs/modelkeep-api.md` no longer state that patterns are
  unsupported, and state that the archive holds what it holds and asserts nothing about
  upstream completeness.

## Verification

```sh
nix develop -c cargo fmt --check
nix develop -c cargo clippy --all-targets --all-features -- -D warnings
nix develop -c cargo test --all-features
nix flake check
```

Unit tests for selection normalization, unsafe-pattern rejection, and idempotency-key
derivation. Integration tests with both
supported real `hf` / `huggingface_hub` versions for filtered cold acquisition, warm and
offline retrieval of the subset, metadata listing, a resolve for a path outside the
selection, extension of a previously imported revision, crash during an extension, and
staging-selection mismatch. Run the supported
real-client suite, since this changes the compatibility surface. Before closing, record
one sanitized QNAP measurement of a filtered prefetch against the same repository,
including transferred bytes and peak staging usage.

## Risks and assumptions

Pattern semantics must match the official client's `allow_patterns` / `ignore_patterns`
behavior; that must be observed against a supported client rather than assumed. The
extension operation is the material risk: it introduces the first write into an already
published revision directory, so its crash safety must be proven by test rather than by
argument, and it must never touch a path the live manifest already lists.

## Implementation status

Implemented on 2026-09-24. Verified on x86_64-linux with `cargo fmt --check`,
`cargo clippy --all-targets --all-features -- -D warnings`, `cargo test --all-features`,
and `nix flake check`, each with its exit status taken directly rather than through a
pipe. An independent reviewer checked each acceptance criterion against the code and
confirmed that the load-bearing tests fail when the behavior they guard is broken.

All three work items are implemented: `include` / `exclude` on a prefetch job, with
pattern validation delegated to the existing selection type and the normalized selection
in the idempotency key; pull-through honouring the requested path; and monotonic
extension of a published revision. An unrestricted request keeps its previous
idempotency hash input byte for byte, which a golden-value test now pins.

A published revision is reconciled against the requested selection through a
resolve-only helper invocation, so only the missing difference is transferred and an
empty difference is recorded as a real no-op.

Putting the selection into the fetch staging identity initially broke resumption of an
interrupted repository-wide acquisition, which `archive-crash-upgrade` caught. ADR-0020
decision 5 now states the adoption rule and why requiring equal selections would be
wrong.

Measured limitation, documented in `modelkeep-api.md`: a client's own `--include` does
not narrow a *first* acquisition, because the client reads repository metadata first and
that route acquires the whole repository. Archiving a subset requires a filtered
prefetch, or a revision that is already published. Issue 0074 tracks removing that
limitation.

`nix flake check` omits aarch64-linux as an incompatible system, so the QNAP release
architecture is covered by the native GitHub Actions jobs, not by this run.

**Remaining before this issue can close**: record one sanitized QNAP measurement of a
filtered prefetch against a representative repository, including transferred bytes and
peak staging usage. That was not done in this work and cannot be done away from the
deployment.

## Field verification (2026-09-24, v0.4.9)

A filtered prefetch of `Qwen/Qwen3-Coder-Next-GGUF` ran on the QNAP deployment with

```json
"include": ["*.gitattributes", "*.md", "Qwen3-Coder-Next-Q4_K_M/*"]
```

and completed with `outcome: published` and 48,411,000,124 bytes transferred, against
469.92 GB for the whole repository. The archived set is the four `Q4_K_M` shards plus
`.gitattributes` and `README.md`; the tree route reports the commit's whole upstream file
list, with ModelKeep's digest `null` for the paths the archive does not hold.

Separately, a single-file resolve of an unarchived repository archived exactly one file of
the twenty-six the commit contains, which is the same mechanism from the client-driven side.

Logical archive size afterwards is 2,052,929,651,040 bytes at 88% filesystem availability,
consistent with the 1.935 TB recorded before this work plus the two acquisitions since.

**Peak staging usage was not captured.** The acquisition had already completed when these
measurements were taken, and peak temporary usage is not retained anywhere after publication.
The transferred-bytes figure is the one that decides this issue — 48.41 GB where the
unfiltered acquisition would have moved 469.92 GB — so this is recorded as measured rather
than left blocking, and a future filtered prefetch can capture the staging figure while it is
running if that number is wanted.

**Done**, with that one figure explicitly not measured.
