---
status: done
priority: P3
created: 2026-09-24
updated: 2026-09-24
---
# Issue 0075: Complete the structured operational event reference

- Status: Done
- Priority: P3
- Related ADR: None

## Objective

Make `docs/structured-operational-events.md` list every event the binary emits, so an
operator building alerting from it is not surprised by an event the reference omits.

## Problem

A mechanical audit on 2026-09-24 compared the events emitted in `src/` against the
reference table and found twenty-two emitted events with no entry. Nearly all predate
the current work; the table has drifted as events were added.

The gap is one-directional and therefore easy to miss: every documented event exists,
so reading the table gives no sign that it is incomplete.

## Acceptance criteria

- Every `event = "..."` emitted in `src/` has an entry with its level and fields.
- The reference states which events are guaranteed for a given operation and which are
  conditional, so an operator can tell a missing event from an impossible one.
- A check fails when an event is emitted without an entry, so the table cannot drift
  again. A test that only reads the table cannot detect the one-directional gap.

## Verification

```sh
nix develop -c cargo fmt --check
nix develop -c cargo clippy --all-targets --all-features -- -D warnings
nix develop -c cargo test --all-features
nix flake check
```

Confirm the new check fails when an undocumented event is introduced, by introducing
one in a scratch copy.

## Risks and assumptions

Matching emitted events by source inspection is approximate for events built through a
helper rather than a literal. The check should fail closed on anything it cannot match
rather than passing silently.

## Implementation status

Implemented on 2026-09-24. A full enumeration of `event = "..."` in `src/` (outside
`#[cfg(test)]` modules) found 49 distinct emitted events against 27 documented at the
start of this task, a gap of exactly 22 — matching the count in the Problem section.
All 22 were added to `docs/structured-operational-events.md`'s table, with fields read
directly from each call site (including the two `configuration_failed` call sites,
which have different fields, and the two `ownership_initialization_failed` sites,
which report `error` or `exit_status` depending on the failure mode). Two new
explanatory sections were added — "`serve` startup sequence" and "Management API job
lifecycle" — stating which events are guaranteed given a prior step succeeded and
which are conditional, based on reading the surrounding control flow rather than
guessing; one gap found while writing this (invalid management/admin configuration
aborts startup without any `configuration_failed` event, only the top-level
`process_failed`) is called out explicitly rather than left implied.

A new fail-closed check, `structured-event-reference` (`tests/structured_event_reference_check.py`,
wired into `flake.nix` as a `checks.<system>.structured-event-reference` derivation),
tokenizes `src/**/*.rs` (comments, string/char/raw-string literals, `#[cfg(test)] mod`
exclusion via brace matching) to find every `tracing::{trace,debug,info,warn,error}!`
call with an `event` field, and fails the build if: (a) any emitted event name has no
table entry, (b) any `event` field's value is not a plain string literal (e.g. a
variable or `format!(...)` — this is the fail-closed path required by the issue), or
(c) any documented event is never emitted outside tests (the optional reverse
direction). Verified the check actually fails, using a copy of the working tree (`.git`
and `target` excluded) built as a path flake: removing one table row, and separately
adding an undocumented `tracing::info!` call and a call with a non-literal `event`
value, each produced `nix build --no-link '.#checks.x86_64-linux.structured-event-reference'`
exit code 1 with a message identifying the exact problem; reverting each change in the
copy restored exit code 0. `archive-audit-cli` and `compose-network-boundary` were
built the same way against the same copy and still exit 0, confirming no existing
check regressed. `cargo fmt --check`, `cargo clippy --all-targets --all-features -- -D
warnings`, and `cargo test --all-features` (188 + 3 tests) all exited 0 against the
real working tree. `nix flake check` was not run against the working tree itself: the
new `tests/structured_event_reference_check.py` is not committed, and this task's
constraints forbid `git add`, so Nix's git-tracked-files-only flake source cannot see
it there; it was instead exercised via the untracked copy above, which is what the
task's own verification section asks for.

No `src/` behavior changed; this was documentation plus a new, additive check.
