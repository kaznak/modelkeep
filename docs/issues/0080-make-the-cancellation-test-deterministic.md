---
status: open
priority: P1
related_adrs:
  - ADR-0021
created: 2026-09-24
updated: 2026-09-24
---
# Issue 0080: Make the cancellation test deterministic

- Status: Open
- Priority: P1
- Related ADR: ADR-0021

## Objective

Find out why `admin::tests::cancelling_a_running_job_stops_its_helper_process` sometimes
fails, and remove the cause rather than the symptom.

## Problem

Observed on 2026-09-24: the test failed once during a `nix flake check` and passed on a
re-run, with no change to the tree. It was added with Issue 0076 and covers cancelling a
running job and stopping the helper process it owns.

A test that fails sometimes is worse than a test that fails always. Everything in the recent
work rests on a failing test meaning something: sensitivity checks that broke the
implementation and watched the right test fail, an idempotency guard whose removal broke
nothing until a test was added for it, and a cancellation that published anyway until a
decisive test caught it. A flaky test teaches the opposite habit — re-run and move on — and
it teaches it precisely where cancellation correctness is being asserted.

## The possibility that matters

The flakiness may be in the test, or it may be in the product. Cancellation races the
acquisition it is cancelling: the token has to own a child process that another thread is in
the middle of spawning and then blocking on. A cancel that arrives in the window before the
child is recorded, or between the child exiting and the commit, is exactly the kind of
interleaving a scheduler will hit occasionally and a test will hit rarely.

So the first question is not "how do we stabilise the test" but "what interleaving did it
find". If the answer is a product race, the test did its job and the product is what needs
fixing.

## Do not

- add a sleep, a retry, a longer timeout, or `#[ignore]` to make it pass;
- weaken what it asserts;
- conclude it is a test-only problem without having identified the interleaving.

## Acceptance criteria

- The interleaving that produced the failure is identified and described, not guessed at.
- If it is a product race, the product is fixed and a test covers that interleaving
  deterministically.
- If it is a test race, the test synchronises on the condition it actually depends on rather
  than on timing.
- The test passes a documented number of consecutive runs, including under parallel load,
  and the number and the method are recorded.
- Whatever the cause, the assertions are the same or stronger afterwards.

## Verification

```sh
nix develop -c cargo fmt --check
nix develop -c cargo clippy --all-targets --all-features -- -D warnings
nix develop -c cargo test --all-features
nix flake check
```

Run the single test in a loop, and run the whole suite repeatedly under CPU contention, to
establish the failure rate before and after. A fix that cannot be shown to change a measured
failure rate has not been shown to be a fix.

## Risks and assumptions

A rare interleaving may not reproduce under a loop on this machine. If it cannot be
reproduced, say so rather than declaring it fixed, and prefer making the code structurally
unable to hit the window over asserting that it no longer does.
