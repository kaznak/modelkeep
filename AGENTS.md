# AGENTS.md — ModelKeep

## Purpose

ModelKeep is a persistent pull-through mirror for model repositories, initially targeting the Hugging Face Hub protocol.

The QNAP archive is durable state. Local client caches, server processes, databases, indexes, and container images are replaceable. A server upgrade or implementation change must never require redownloading already archived model data.

Read `DEVELOPMENT_PLAN.md` before making architectural changes. Read applicable records under `docs/adr/` before changing a recorded decision.

## Core invariants

These are hard constraints unless the user explicitly requests an architectural change and the corresponding ADR is updated.

1. Archived model revisions are immutable once published.
2. Requested mutable refs such as `main` are resolved to commit IDs; updating a ref must not destroy the old revision.
3. Model files are stored as ordinary materialized files. ModelKeep must not make an opaque/version-specific cache format the sole durable representation.
4. Archive deletion is explicit. Do not implement automatic GC of archived revisions.
5. A completed archive object is published only after successful download/import and integrity checks.
6. Partial data must never be observable as a completed object.
7. ModelKeep does not implement the Xet/CAS protocol. Upstream acquisition may delegate to the official Hugging Face client.
8. A mirrored revision must remain downloadable when upstream Hugging Face access is unavailable.
9. Metadata is reconstructible from durable archive state where practical. Do not make SQLite or another index the only source of truth for model bytes.
10. Never redirect a client in a way that silently bypasses ModelKeep and downloads model payloads directly from upstream/Xet.

## Scope discipline

Implement the smallest behaviorally complete change required by the current task.

Do not:
- turn ModelKeep into a complete Hugging Face Hub clone;
- add upload/push, Spaces, inference APIs, web UI, automatic archive GC, compression, git-annex, or multi-storage placement unless explicitly requested;
- perform unrelated refactors, renames, formatting sweeps, dependency upgrades, or file moves;
- introduce an abstraction merely because it might be useful later.

If a requested change conflicts with an ADR or core invariant, stop and report the conflict before implementation unless the task explicitly asks to change that decision.

## Task protocol

For every non-trivial task, determine and record before editing:

- **Objective** — one concrete outcome.
- **Write scope** — files/modules expected to change.
- **Do not touch** — relevant protected or unrelated areas.
- **Done when** — externally observable acceptance criteria.
- **Verification** — exact commands/tests proving completion.
- **Risks/assumptions** — only material unknowns.

Keep tasks small enough that one change can be reviewed independently. If the task contains separable architectural and implementation changes, perform the architectural decision first.

Do not claim completion from code inspection alone when executable verification is available.

## Protocol compatibility

Hugging Face compatibility must be evidence-driven.

- Do not guess HTTP methods, headers, redirects, ETags, Range behavior, revision semantics, or client behavior.
- When compatibility behavior is uncertain, reproduce it against a supported real `huggingface_hub` / `hf` client and turn the observation into an integration test.
- Prefer black-box compatibility tests over coupling ModelKeep to private client internals.
- When supporting a new client version changes observed behavior, preserve the regression as a test.
- Keep upstream protocol observations separate from ModelKeep policy decisions.

## Durable writes

Any code that creates archive content must follow the equivalent of:

1. resolve the immutable revision;
2. write into a staging/partial location;
3. complete acquisition;
4. validate expected size and available digest/checksum information;
5. flush data as required for the durability boundary;
6. atomically publish the completed object/revision;
7. update reconstructible metadata/ref state.

Never overwrite a published immutable revision in place.

Crash tests must cover interruption before publication. On restart, ModelKeep must not serve a partial file as complete.

## Concurrency

Concurrent requests for the same missing object/revision should use single-flight behavior where practical.

Correctness is more important than maximum concurrency. Avoid holding global locks across network or large-file I/O. Concurrency changes require tests for duplicate fetch prevention and failure propagation.

## Security

Treat repository IDs, revisions, filenames, HTTP paths, headers, tokens, and imported cache contents as untrusted input.

At minimum:
- prevent path traversal and escaping the archive root;
- do not log bearer tokens, credentials, signed URLs, or sensitive headers;
- reject malformed/unsafe archive paths;
- run the container as non-root;
- require no Linux capabilities by default;
- keep the container root filesystem read-only where deployment permits;
- never require Docker socket access;
- bind/write only the intended archive/state paths.

Security checks must not be disabled merely to make compatibility tests pass.

## Error handling

Distinguish operationally meaningful failures, including:
- upstream unavailable;
- upstream object/revision not found;
- authorization failure;
- integrity mismatch;
- invalid Range;
- unsafe path;
- disk full / I/O failure;
- interrupted/partial acquisition;
- internal metadata/index failure.

Do not silently convert integrity or storage failures into cache misses.

## Dependencies

Prefer the standard library and existing dependencies.

A new runtime dependency must have a concrete current use. Avoid dependencies for speculative future work. For upstream Hugging Face/Xet acquisition, prefer delegation to the official supported HF client rather than reimplementing Xet.

Nix is the canonical reproducible build/deployment definition. Development shortcuts must not become the only way to build or test the project.

## Testing requirements

Every behavior change requires an appropriate test unless it is purely documentary.

Maintain these classes of tests:

- unit tests for pure parsing/path/Range/manifest logic;
- integration tests using real supported `hf` / `huggingface_hub` clients against ModelKeep;
- cold-miss tests: upstream -> ModelKeep archive -> client;
- warm-hit/offline tests: upstream blocked, archived revision still downloads;
- Range/HEAD tests for large-file semantics;
- concurrency tests for single-flight behavior;
- crash/interruption tests for atomic publication;
- immutable-revision/ref-update tests;
- import tests for existing Hugging Face caches;
- security tests for traversal and credential leakage.

Do not weaken assertions, skip tests, or change expected results solely to make a failing implementation pass. If expected behavior legitimately changes, explain why and update the owning specification/ADR where applicable.

## Required validation

Use repository-provided commands as they become available. The intended baseline is:

```sh
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
nix flake check
```

Run focused tests during development and the relevant full checks before declaring a task complete. If a check cannot run, report the exact reason and what remains unverified.

For protocol-affecting changes, run the HF client integration suite in addition to unit tests.

## Nix and container rules

- Support `aarch64-linux` as a first-class target for QNAP deployment.
- Build the OCI image from the flake.
- Pin dependencies through Nix/lockfiles.
- Prefer a minimal runtime closure, but never trade away correctness or supported HF compatibility merely to reduce image size.
- Use immutable/versioned image tags for deployment; do not rely on `latest` as the deployment identity.
- Container state belongs under explicit mounted paths, not the image filesystem.

## Documentation and ADRs

`AGENTS.md` defines development behavior, not the complete architecture.

Create or update an ADR when changing a durable architectural decision, especially:
- archive representation;
- immutability semantics;
- upstream acquisition strategy;
- Xet boundary;
- deletion/GC policy;
- metadata source of truth;
- compatibility surface;
- security/trust boundary.

Do not rewrite an accepted ADR to hide history. Supersede it with a new ADR and mark the old record superseded.

## Completion report

At the end of a non-trivial task, report concisely:

1. what changed;
2. files/modules changed;
3. tests/checks run and results;
4. material assumptions or remaining risks;
5. any follow-up that is genuinely required.

Do not describe unexecuted tests as passing.
