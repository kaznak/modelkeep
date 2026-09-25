---
status: open
priority: P2
related_adrs:
  - ADR-0003
  - ADR-0022
created: 2026-09-26
updated: 2026-09-26
---
# Issue 0089: Decide how llama.cpp gets a file the archive does not hold yet

- Status: Open
- Priority: P2
- Related ADR: ADR-0003, ADR-0022

## Objective

Decide, from measurement, whether and how a llama.cpp download of a file the archive
does not hold can succeed without an operator prefetching it first.

## Problem

Reported from the GX10 deployment on 2026-09-26. A cold `resolve` answers `503` with
`Retry-After` after the cold-miss deadline (Issue 0069), and the acquisition continues.
Both pinned `huggingface_hub` versions retry that. llama.cpp v0.4.1, as read from its
sources by the reporter, issues one `HEAD` and fails on any non-2xx without honoring
`Retry-After`. llama-swap then restarts llama-server, which issues the `HEAD` again and
joins the running acquisition, so the model starts only after the acquisition finishes
and after an unbounded number of failed starts.

The reporter asked whether a cold `HEAD` could answer `200` with `Content-Length` and
`ETag` and make the `GET` wait instead. That conflicts with what ModelKeep currently
promises: the `ETag` is a digest of bytes ModelKeep holds and verified, and upstream's
LFS sha256 is not that; a `GET` that waits beyond the client's read timeout fails
anyway; and a non-LFS file has no upstream digest at all.

## Scope

- Observe llama.cpp's actual behavior against a stand-in (timeouts on `HEAD` and `GET`,
  whether it resumes `.downloadInProgress`, what `ETag` it stores).
- Decide between documenting prefetch as the supported path for llama.cpp, a
  llama.cpp-specific answer, or no change, and record the decision.

## Acceptance criteria

- The decision and the measurement it rests on are recorded; any behavior change is
  covered by a test with a real llama.cpp client or a recorded observation.

## Verification

Reproduce against a controlled upstream fixture and record the observation under
`docs/observations/`.

## Risks and assumptions

llama.cpp is not a pinned client; its behavior is only known from the report so far.
Workaround until decided: prefetch through the Admin API before starting llama-server.
