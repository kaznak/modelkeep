---
status: open
priority: P3
created: 2026-09-24
updated: 2026-09-24
---
# Issue 0075: Complete the structured operational event reference

- Status: Open
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
