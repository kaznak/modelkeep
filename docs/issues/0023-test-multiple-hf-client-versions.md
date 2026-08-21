---
status: open
priority: P1
related_adrs:
  - ADR-0005
created: 2026-08-21
updated: 2026-08-21
---
# Issue 0023: Test multiple supported Hugging Face client versions

- Status: Open
- Priority: P1
- Related ADR: ADR-0005

## Objective

Detect protocol regressions across an explicit supported `huggingface_hub` version
range in CI.

## Problem

CI currently tests the one version pinned by nixpkgs, while the development plan
requires compatibility coverage for multiple client versions.

## Scope

- Define a small, explicit supported-version policy.
- Run the same cold-miss, warm/offline, HEAD, Range, and no-bypass suite for each.
- Keep deterministic tests separate from optional online observation jobs.

## Acceptance criteria

- CI fails when any supported client version breaks the shared compatibility suite.
- Versions and update procedure are documented and reproducibly pinned.

## Verification

```sh
nix flake check
```

Run every version-matrix job on native amd64 and arm64 GitHub runners.

## Risks and assumptions

Avoid an unbounded historical matrix; support only versions with a concrete operator
need and retire them explicitly.
