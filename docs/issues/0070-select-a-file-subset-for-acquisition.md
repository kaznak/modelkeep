---
status: open
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

- Status: Open
- Priority: P0
- Related ADR: ADR-0020 (proposed, governs this work), ADR-0008 (partially superseded),
  ADR-0002, ADR-0005, ADR-0010, ADR-0015, ADR-0017, ADR-0018

## Objective

Acquire and serve an explicitly requested subset of a repository's files through the
management API and through pull-through, without any revision claiming a coverage it
does not have, and without a later request for an unlisted path costing a whole
repository.

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
returns an error.

Honouring the requested path in pull-through therefore requires adding files to an
already published revision, which requires the monotonic extension operation. The four
work items below are one reviewable unit.

## Governing decision

ADR-0008 forbade a file filter and deferred per-file acquisition. ADR-0020 (proposed)
specifies the deferred semantics and partially supersedes it: a manifest records
`coverage: "snapshot" | "selection"` plus the normalized selection; `complete` keeps its
existing publication-integrity meaning, so the four serving gates are unchanged;
coverage is consulted by acquisition; and a selection-scoped revision's file set may
grow but never change.

**This issue is blocked on ADR-0020 being accepted.** If a decision there changes, this
issue changes with it.

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

2. **Manifest coverage.** `write_manifest` (`src/lib.rs:1615`, three call sites) records
   `coverage` and the selection. Readers treat a missing `coverage` as `"snapshot"`.

3. **Pull-through honours the requested path.** `fetch_and_publish` stops discarding the
   caller's file list (`src/pullthrough.rs:342`), so a miss acquires the requested path
   rather than the repository.

4. **Monotonic extension.** Adding files to an already published revision: write the new
   files durably, then atomically replace the manifest with a superset. The existing
   `fs::rename`-onto-`revisions/<commit>` publication cannot be reused. Extending a
   `coverage: "snapshot"` revision is refused.

## Write scope

`src/admin.rs`, `src/pullthrough.rs`, `src/lib.rs` (manifest and extension),
`src/upstream.rs`, `src/http.rs` where the requested path is passed,
`docs/admin-api.md`, `docs/modelkeep-api.md`.

## Do not touch

- archive deletion or GC policy;
- the Xet boundary and delegation to the official client;
- the four serving gates' meaning of `complete`;
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
- The published revision records `coverage: "selection"` with the normalized selection
  and `complete: true`, and is served through the unchanged serving gates.
- A real supported client downloads the archived subset while upstream is unavailable,
  and repository metadata reports the archived set and the selection scope.
- A resolve for a path outside the selection acquires that path only, extends the
  existing revision, and does not re-transfer the repository; the extension never
  overwrites or removes a published path.
- An induced crash during an extension leaves either the old or the new manifest live,
  and no file absent from the live manifest is ever served.
- Extending a `coverage: "snapshot"` revision is refused with an operationally
  meaningful error.
- Interrupting a filtered acquisition publishes nothing partial; a retry adopts only
  staging whose recorded selection matches, and staging under a different selection is
  discarded rather than resumed.
- `docs/admin-api.md` and `docs/modelkeep-api.md` no longer state that patterns are
  unsupported, and describe coverage and its serving semantics.

## Verification

```sh
nix develop -c cargo fmt --check
nix develop -c cargo clippy --all-targets --all-features -- -D warnings
nix develop -c cargo test --all-features
nix flake check
```

Unit tests for selection normalization, unsafe-pattern rejection, idempotency-key
derivation, manifest coverage round-tripping, legacy manifests with no `coverage` field,
and refusal to extend a snapshot-coverage revision. Integration tests with both
supported real `hf` / `huggingface_hub` versions for filtered cold acquisition, warm and
offline retrieval of the subset, metadata listing, a resolve for a path outside the
selection, crash during an extension, and staging-selection mismatch. Run the supported
real-client suite, since this changes the compatibility surface. Before closing, record
one sanitized QNAP measurement of a filtered prefetch against the same repository,
including transferred bytes and peak staging usage.

## Risks and assumptions

Pattern semantics must match the official client's `allow_patterns` / `ignore_patterns`
behavior; that must be observed against a supported client rather than assumed. The
extension operation is the material risk: it introduces the first write into an already
published revision directory, so its crash safety must be proven by test rather than by
argument, and it must never touch a path the live manifest already lists.
