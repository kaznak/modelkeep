---
status: open
priority: P1
related_adrs:
  - ADR-0003
  - ADR-0005
created: 2026-08-21
updated: 2026-08-21
---
# Issue 0022: Complete the protocol observation matrix

- Status: Open
- Priority: P1
- Related ADR: ADR-0003, ADR-0005

## Objective

Record and preserve the actual supported-client behavior for public, safetensors,
sharded, revision-specific, redirecting, and Xet-backed model downloads.

## Problem

The deterministic fixture covers the current compatibility subset, but the complete
protocol-observation matrix in the development plan has not been captured against
representative upstream repositories.

## Scope

- Trace request methods, paths, relevant headers, redirects, Range, and commit data.
- Keep captured observations separate from ModelKeep policy.
- Convert every required behavior into deterministic regression fixtures where legal
  and practical; never allow payload redirects to bypass ModelKeep.

## Acceptance criteria

- Every model category in the development plan has a dated observation record.
- Required behavior is represented by black-box tests using a supported real client.
- Xet/CDN redirect observations do not introduce direct client bypass.

## Verification

```sh
nix develop -c cargo test --all-features
nix flake check
```

Run the documented observation procedure for every matrix row and the real HF client
integration suite against the resulting deterministic fixtures.

## Risks and assumptions

Upstream behavior and public fixtures change. Records must identify client versions,
repository revisions, and observation dates without committing model payloads.
