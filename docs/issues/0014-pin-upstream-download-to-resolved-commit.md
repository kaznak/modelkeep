---
status: open
priority: P0
related_adrs:
  - ADR-0002
  - ADR-0005
  - ADR-0008
created: 2026-08-21
updated: 2026-08-21
---
# Issue 0014: Pin upstream download to the resolved commit

- Status: Open
- Priority: P0
- Related ADR: ADR-0002, ADR-0005, ADR-0008

## Objective

Guarantee that the bytes published under a commit ID were downloaded from that exact
commit even if a mutable upstream ref moves during acquisition.

## Problem

The helper currently resolves `args.revision` with `repo_info`, then independently
passes the original revision to `snapshot_download`. If `main` moves between those
operations, ModelKeep can publish bytes from commit B beneath commit A, violating the
immutable revision identity and integrity contract.

## Scope

- Resolve the requested revision once through the official Hugging Face client.
- Download the snapshot using the returned immutable commit ID.
- Reject empty or malformed commit identities before publication.
- Preserve the originally requested ref separately for mutable-ref updates.

## Acceptance criteria

- A controlled ref-movement test proves that the downloaded bytes match the commit
  recorded in the manifest and publication path.
- A mutable ref moving during acquisition cannot produce a mixed or mislabeled
  revision.
- Existing immutable revisions are never overwritten.

## Verification

```sh
nix develop -c cargo test --all-features
nix develop -c cargo clippy --all-targets --all-features -- -D warnings
nix flake check
```

Run the supported-client acquisition integration test with a controllable upstream
fixture that moves a ref between resolution and download.

## Risks and assumptions

The official client must accept the resolved commit ID for `snapshot_download`.
Protocol observations must be captured in a black-box regression test rather than
coupling ModelKeep to private client internals.
