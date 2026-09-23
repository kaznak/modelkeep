---
status: open
priority: P2
related_adrs:
  - ADR-0020
  - ADR-0005
created: 2026-09-24
updated: 2026-09-24
---
# Issue 0074: Answer repository metadata without acquiring the repository

- Status: Open
- Priority: P2
- Related ADR: ADR-0020, ADR-0005

## Objective

Let a client's own file filter narrow a first acquisition, by answering repository
metadata for an unarchived revision without first acquiring the whole repository.

## Problem

A supported client reads repository metadata before it requests any file. For a
revision the archive has never seen, the metadata routes acquire the entire repository
and answer from the archive. The client's `--include` / `allow_patterns` is applied to
a file list that ModelKeep has already paid to acquire in full.

Measured with both pinned clients on 2026-09-24: a filtered download of an unseen
repository archives the full snapshot.

So the subset feature delivered by Issue 0070 is reachable only through a filtered
`prefetch` job, or against a revision that is already published, where a request for an
absent path extends it. The obvious client-side spelling of the same intent silently
costs the whole repository. For `Qwen/Qwen3-Coder-Next-GGUF` that is 469.92 GB instead
of 48.41 GB.

## Scope

Answer metadata for an unarchived revision from upstream repository information rather
than from an acquisition, and let the per-file requests that follow acquire only what
the client asks for.

Points to settle before implementing:

- What metadata is served when upstream is unavailable and the revision is unarchived.
  An archived revision must keep answering from the archive (core invariant 8).
- Whether a metadata answer that was not derived from archived state is distinguishable
  to an operator, and whether it is cached at all.
- That this stays metadata only. Payload must never be served from, or redirected to,
  upstream (core invariant 10).
- How it interacts with the metadata cold-miss policy, which currently waits for the
  acquisition by default because the supported clients retry nothing on these routes.

## Acceptance criteria

- A filtered download of an unseen repository through a real supported client archives
  only the matching files.
- An archived revision answers metadata without contacting upstream.
- An unarchived revision with upstream unavailable fails in an operationally meaningful
  way and does not publish or serve fabricated metadata.
- Payload acquisition still goes through ModelKeep; no redirect to upstream or Xet.

## Verification

```sh
nix develop -c cargo fmt --check
nix develop -c cargo clippy --all-targets --all-features -- -D warnings
nix develop -c cargo test --all-features
nix flake check
```

Extend the supported-client integration checks with a filtered download of an unseen
repository, asserting the archived set is the filtered set.

## Risks and assumptions

Serving metadata that is not backed by archived state is a change to what a metadata
answer means, and it must not become a path by which ModelKeep reports files it cannot
deliver. If that cannot be made safe, the alternative is to keep the current behavior
and document the filtered prefetch as the only supported way to archive a subset.
