---
status: open
priority: P1
related_adrs:
  - ADR-0008
  - ADR-0020
created: 2026-09-24
updated: 2026-09-24
---
# Issue 0072: Import must not assert unverified snapshot completeness

- Status: Open
- Priority: P1
- Related ADR: ADR-0008, ADR-0020

## Objective

Stop `import-hf-cache` from recording an unverified file set as a complete upstream
snapshot.

## Problem

`import_hf_cache` walks each `snapshots/<commit>` directory of a local Hugging Face
cache and publishes exactly the files it finds (`src/importer.rs`), and `write_manifest`
emits `"complete":true` unconditionally (`src/lib.rs:1625`). Nothing compares the
imported file set against upstream repository metadata for that commit.

A cache produced by `hf download <repo> --include '<pattern>'`, or any cache whose
download was interrupted, therefore becomes a revision that claims to be a complete
snapshot. ModelKeep then serves it as complete, and `is_complete_revision_for_type`
reports it as complete to the pull-through readiness check.

This is the exact failure ADR-0008 was written to prevent — "publishing the requested
file as the revision would make later requests mistake a partial acquisition for a
complete archive" — occurring at the import boundary rather than the acquisition
boundary. It also interacts with Issue 0070: a mislabeled import claims
`coverage: "snapshot"`, and ADR-0020 refuses to extend such a revision, so a later
request for a file the cache never held cannot be satisfied by extension.

## Reproduction (executed 2026-09-24, v0.4.7 debug binary)

A cache holding only one quantisation directory, of the shape `hf download --include`
produces:

```text
cache/models--org--gguf/snapshots/<40-hex commit>/Qwen3-Q4_K_M/model-00001.gguf
cache/models--org--gguf/refs/main
```

```sh
modelkeep import-hf-cache <cache> <archive>
# imported 1 revisions and 1 refs
```

The resulting `.modelkeep-manifest.json` is:

```json
{"version":1,"complete":true,"repo_type":"model","repo_id":"org/gguf",
 "commit":"<40-hex commit>","files":[{"path":"Qwen3-Q4_K_M/model-00001.gguf", ...}]}
```

One file out of a repository is recorded as a complete snapshot, and `refs/main` points
at it.

## Write scope

- `src/importer.rs` completeness determination;
- `src/lib.rs` manifest scope emission, shared with Issue 0070;
- an explicit administrative path for re-labelling already imported revisions;
- `docs/admin-api.md` or the deployment documentation that owns the import procedure.

## Do not touch

- imported bytes; no re-download and no rewrite of archived files;
- automatic correction of existing revisions. Re-labelling is an explicit
  administrative action, per core invariant 4 and ADR-0007.

## Acceptance criteria

- An import that cannot verify the file set against upstream metadata for the commit
  records the revision with `coverage: "selection"` under ADR-0020, not as a snapshot.
- An import that does verify the file set records `coverage: "snapshot"`, and the check
  is exercised by a test.
- Import remains possible with upstream unavailable; it records the weaker scope rather
  than failing, and says so in its report.
- Revisions imported before this change keep their recorded value. An explicit
  administrative command can re-evaluate and re-label them, and it is documented.
- `ImportReport` distinguishes verified-snapshot from selection-scoped imports.

## Verification

```sh
nix develop -c cargo fmt --check
nix develop -c cargo clippy --all-targets --all-features -- -D warnings
nix develop -c cargo test --all-features
nix flake check
```

Add tests importing a cache produced with `--include` against fixture upstream
metadata, an interrupted cache, a verified complete cache, and an import with upstream
unavailable. Confirm that a selection-scoped imported revision is served for the paths
it holds and triggers acquisition for the paths it does not.

## Risks and assumptions

Verification requires upstream repository metadata for the commit, which may be
unavailable exactly when an import is most useful. The decision above therefore
degrades the recorded scope instead of refusing the import. This issue depends on
ADR-0020 for the coverage representation; if ADR-0020 is rejected, the fallback is to
define a separate honest representation for an unverified import rather than to keep
asserting a snapshot.
