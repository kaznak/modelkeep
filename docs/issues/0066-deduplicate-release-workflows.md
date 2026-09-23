---
status: in-progress
priority: P1
related_adrs: []
created: 2026-09-23
updated: 2026-09-23
---
# Issue 0066: Deduplicate release workflows for one commit

- Status: In Progress
- Priority: P1
- Related ADR: None

## Objective

Run the expensive native amd64/arm64 validation and image build only once when the
same commit is pushed to `main` and immediately tagged for release.

## Problem

The release workflow is triggered separately by the `main` push and the `v*` tag
push. Each run starts both native architecture jobs and executes the complete flake
checks, so a normal release can occupy four runners with duplicate work. Architecture
jobs and independent Nix derivations are already parallelized within each run.

## Write scope

- `.github/workflows/release-image.yml` concurrency policy;
- focused documentation or validation only if required.

## Do not touch

- test coverage, supported architectures, image publication conditions, or release
  tags;
- Nix build definitions and archive behavior.

## Acceptance criteria

- Workflow runs for the same commit share one concurrency group.
- A newer tag-triggered run cancels an in-progress main-triggered duplicate.
- Ordinary main pushes and release tags still run both native architecture jobs.
- Only a successful tag run publishes the versioned multi-architecture manifest.

## Verification

```sh
nix flake check
```

Inspect the parsed workflow and confirm that concurrency is keyed by commit SHA with
`cancel-in-progress` enabled. Confirm behavior on the next release event.

## Risks and assumptions

The release procedure pushes `main` before its tag, so the tag run is normally newer
and becomes the surviving run. If the main run has already completed before tagging,
GitHub Actions cannot retroactively reuse it and the tag run still validates again.

## Implementation status

Implemented workflow-level concurrency keyed by `github.sha`, with
`cancel-in-progress: true`. `actionlint` accepts the workflow. Keep the issue open
until the next main-then-tag release demonstrates that the tag run survives and the
duplicate main run is cancelled while both architecture jobs still run in the
surviving workflow.
