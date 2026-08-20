---
status: open
priority: P1
related_adrs:
  - ADR-0004
created: 2026-08-21
updated: 2026-08-21
---

# Issue 0008: Fix remove CLI option parsing

- Status: Open
- Priority: P1
- Related ADR: ADR-0004

## Objective

Make `modelkeep remove` correctly distinguish real deletion, dry-run, malformed options, and missing arguments while remaining fail-safe.

## Problem

The reviewed argument handling can reject the no-option real-delete path while accepting `--dry-run`. This fails safe, but leaves the destructive administrative path inconsistent and insufficiently exercised.

## Scope

* No optional flag performs the explicitly requested deletion.
* `--dry-run` reports the deletion plan without mutation.
* Unknown options and missing arguments fail before mutation.
* Keep deletion narrowly scoped and consistent with the no-automatic-GC policy.

## Acceptance criteria

* Dry-run changes no files or refs.
* The documented real-delete invocation removes exactly the intended revision/state.
* Unknown flags and missing arguments cannot delete anything.
* One revision cannot implicitly delete unrelated revisions.
* Behavior for refs targeting a removed revision is explicitly defined and tested.

## Verification

    cargo test --all-features
    cargo clippy --all-targets --all-features -- -D warnings

Add CLI-level tests using temporary archives for dry-run, real deletion, unknown options, missing arguments, a non-existent revision, and ref interaction.

## Risks and assumptions

Deletion is intentionally explicit under ADR-0004. Tests must not weaken that boundary merely to simplify CLI behavior.
