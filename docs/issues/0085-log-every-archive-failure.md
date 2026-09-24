---
status: open
priority: P2
created: 2026-09-24
updated: 2026-09-24
---
# Issue 0085: Log every archive failure, not two of them

- Status: Open
- Priority: P2
- Related ADR: None

## Objective

Make an archive operation that fails leave a trace, whatever it failed with.

## Problem

`log_archive_failure` in `src/pullthrough.rs` logs an integrity mismatch and an I/O error,
and silently drops everything else:

```rust
match &error {
    ArchiveError::IntegrityMismatch(_) => tracing::warn!(event = "archive_verification_failed", ...),
    ArchiveError::Io(io_error) => tracing::error!(event = "archive_storage_failed", ...),
    _ => {}
}
error.into()
```

`AlreadyPublished`, `InvalidPath` and `ReferencedRevision` take the `_` arm. The function's
name says it logs a failure; for those it converts and returns one.

Observed on the deployment on 2026-09-24: a prefetch failed with `error_class: "conflict"`
and the container logs contained exactly two lines for that job — the progress event and the
terminal failure. Nothing said where the conflict came from or what path it concerned. That
silence is why identifying the cause took reading the source and reasoning backwards from
which code paths can return that variant at all. Issue 0083 covers the defect that produced
the conflict; this one covers not being able to see it.

## Scope

- Every variant reaching `log_archive_failure` emits an event with its class, the operation,
  and enough identity to locate it.
- The `_` arm goes away, so a new `ArchiveError` variant cannot be added into silence.

## Acceptance criteria

- Each `ArchiveError` variant passed to `log_archive_failure` produces an event naming its
  class and the operation.
- The events appear in `docs/structured-operational-events.md`, which the existing
  `structured-event-reference` check enforces.
- A test covers each variant rather than asserting that some event was emitted for one of
  them.
- Adding a variant without a logging arm fails to compile, or fails a test, rather than
  compiling into `_ => {}`.

## Verification

```sh
nix develop -c cargo fmt --check
nix develop -c cargo clippy --all-targets --all-features -- -D warnings
nix develop -c cargo test --all-features
nix flake check
```

## Risks and assumptions

An archived path in a log line is not sensitive in the way a token is, and the management
plane already reports repository and revision names, so naming the path an operation failed
on adds no new class of exposure. The value is that the next conflict is diagnosable from the
logs instead of from the source.
