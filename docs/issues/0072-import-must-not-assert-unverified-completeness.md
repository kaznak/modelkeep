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
emits `"complete":true` unconditionally (`src/lib.rs:1625`). Nothing establishes that
the cache held every upstream file for that commit, and nothing could: the import reads
a local directory, and the files it is missing are exactly the ones it has no record of.

A cache produced by `hf download <repo> --include '<pattern>'`, or any cache whose
download was interrupted, therefore becomes a revision that claims to be a complete
snapshot. ModelKeep then reports that coverage to clients, so an unfiltered client
download retrieves the subset while the archive asserts it holds the repository.

This is the failure ADR-0008 was written to prevent — "publishing the requested file as
the revision would make later requests mistake a partial acquisition for a complete
archive" — occurring at the import boundary rather than the acquisition boundary.

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

- `src/importer.rs` coverage determination;
- `src/lib.rs` manifest coverage emission, shared with Issue 0070;
- `docs/admin-api.md` or the deployment documentation that owns the import procedure.

## Do not touch

- imported bytes; no re-download and no rewrite of archived files;
- revisions imported before this change. Under ADR-0020 extension is unconditional, so a
  legacy revision is corrected when a missing file is first requested; no migration pass
  and no upstream audit is required.

## Acceptance criteria

- An import records `coverage: "partial"` under ADR-0020. It never asserts a snapshot.
- `ImportReport` states that imported revisions are recorded as partial.
- Import remains possible with upstream unavailable, because it never contacts upstream.
- A later request for a file an imported revision does not hold extends that revision
  rather than re-acquiring the repository.
- The import procedure documentation states what coverage an import records and why.

## Verification

```sh
nix develop -c cargo fmt --check
nix develop -c cargo clippy --all-targets --all-features -- -D warnings
nix develop -c cargo test --all-features
nix flake check
```

Add tests importing a cache produced with `--include`, an interrupted cache, and a cache
holding a full repository; all are recorded as partial. Add a test that a request for a
file the imported revision lacks extends it and serves the result.

## Risks and assumptions

This issue depends on ADR-0020 for the coverage representation and for unconditional
extension. If ADR-0020 is rejected, the fallback is to define a separate honest
representation for an unverified import rather than to keep asserting a snapshot.
