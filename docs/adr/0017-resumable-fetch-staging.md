# ADR-0017: Resume identified fetch staging after lease expiry

- Status: Accepted
- Date: 2026-09-23
- Supersedes: ADR-0009

## Context

Deleting every expired fetch staging directory is safe, but forces large snapshots
to restart after a container recreation. The official Hugging Face client can reuse
its `local_dir` state and continue an interrupted download.

## Decision

Fetch staging stores versioned, credential-free identity containing the repository,
requested revision, fetch selection, and resolved immutable commit. Recovery keeps
expired fetch staging only when this metadata is valid. A matching retry claims it
exclusively by atomically renaming the directory and renewing its lease; active,
ambiguous, mismatched, and malformed staging is never adopted.

The resumed helper is pinned to the recorded commit. Its output still passes the
normal complete-snapshot validation, manifest, flush, and atomic publication
boundary. The metadata and downloader cache are removed before publication.

Leases expire after 120 seconds and are refreshed every 30 seconds. This bounds the
normal post-recreation wait while retaining a multi-process ownership margin.

## Consequences

- A container recreation can retain already downloaded bytes without treating them
  as durable or complete model data.
- A retry can briefly report a conflict until the previous lease expires.
- Resumption depends on supported Hugging Face client `local_dir` behavior and is an
  optimization; archived ordinary files and manifests remain the authority.
- Invalid identified staging follows normal expired-staging cleanup, while missing
  or unrecognizable lease metadata remains preserved for manual inspection.
