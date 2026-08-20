# ModelKeep task brief template

Use this for non-trivial Luna/agent tasks.

## Objective

One concrete outcome.

## Context

Only the context needed for this task. Link to the relevant development-plan section and ADRs.

## Write scope

- `path/to/file`
- `path/to/module`

## Do not touch

- unrelated subsystems
- archive layout unless explicitly in scope
- accepted ADRs unless the task is to change the decision

## Done when

State externally observable acceptance criteria.

## Verification

```sh
# focused tests first
...

# relevant full validation
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
nix flake check
```

## Risks / assumptions

List only material unknowns.

## Completion evidence

At completion, report changed files, commands actually run, results, and remaining risks.
