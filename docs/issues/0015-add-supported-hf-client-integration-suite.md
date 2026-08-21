---
status: open
priority: P0
related_adrs:
  - ADR-0003
  - ADR-0005
  - ADR-0008
created: 2026-08-21
updated: 2026-08-21
---
# Issue 0015: Add the supported Hugging Face client integration suite

- Status: Open
- Priority: P0
- Related ADR: ADR-0003, ADR-0005, ADR-0008

## Objective

Prove the production client path with a pinned supported `hf` / `huggingface_hub`
version before QNAP deployment.

## Problem

Current HTTP and pull-through tests use in-process requests and fake fetchers. They do
not prove that the real supported Hugging Face client can discover metadata, download
a complete snapshot, use byte ranges and HEAD correctly, or stay behind ModelKeep
when upstream access is removed.

## Scope

- Start ModelKeep in an isolated integration-test environment.
- Exercise the pinned `hf` or `huggingface_hub` client against `HF_ENDPOINT`.
- Cover cold miss, warm hit, explicit commit, mutable ref, HEAD, Range, and concurrent
  client requests.
- Block upstream connectivity for the warm-hit phase and prove the archived revision
  remains downloadable.
- Detect any redirect or payload request that bypasses ModelKeep for upstream/Xet.
- Expose the suite as a distinct flake check and CI step.

## Acceptance criteria

- A real client downloads a complete multi-file revision on a cold miss.
- After deleting the client cache and blocking upstream, the same revision downloads
  successfully from ModelKeep.
- No client payload request is redirected to Hugging Face CDN or Xet CAS.
- Failure output identifies the supported client version and failing protocol step.

## Verification

```sh
nix flake check
nix develop --ignore-environment -c hf version
```

Run the new integration check on both amd64 and arm64 GitHub Actions runners. Tests
that require the public Hugging Face service must be explicitly separated from fully
offline fixture-based tests.

## Dependencies

Issue 0014 must prevent mutable-ref resolution races before a live mutable ref is
accepted as a preservation test.

## Risks and assumptions

Live upstream tests can be rate-limited or changed externally. Keep deterministic
protocol coverage on a controlled fixture and use a small public repository only for
a separately identified compatibility smoke test.
