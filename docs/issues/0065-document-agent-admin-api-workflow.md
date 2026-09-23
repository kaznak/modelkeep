---
status: in-progress
priority: P1
related_adrs:
  - ADR-0015
  - ADR-0018
created: 2026-09-23
updated: 2026-09-23
---
# Issue 0065: Document the agent Admin API workflow

- Status: In Progress
- Priority: P1
- Related ADR: ADR-0015, ADR-0018

## Objective

Provide a versioned Admin API reference and a portable ModelKeep skill so an AI agent
can inspect inventory, submit supported jobs, and monitor them without using the
browser UI or embedding deployment-specific endpoints.

## Write scope

- a tracked Admin API guide under `docs/`;
- a project skill under `.agents/skills/`;
- discoverability links from the main documentation.

## Do not touch

- Admin API behavior or authorization policy;
- deployment endpoints, credentials, operator identity, or site inventory;
- unsupported deletion or file-selection behavior.

## Acceptance criteria

- The guide documents every current `/api/admin/v1` route, request body, paging,
  idempotency, CSRF, terminal states, and safe polling behavior.
- Examples obtain the endpoint from a local ignored configuration or an explicit
  environment variable; tracked files contain no real deployment URL or credential.
- The skill tells agents to inspect before mutating, use immutable revisions where
  practical, avoid duplicate jobs, stop on terminal failure, and never infer success
  from HTTP `202` alone.
- The skill validates with the standard Agent Skills validator.

## Verification

```sh
python3 /path/to/skill-creator/scripts/quick_validate.py \
  .agents/skills/modelkeep-admin
git diff --check
```

Review all documented routes against `src/admin.rs` and verify that no internal
hostname or credential appears in tracked content.

## Risks and assumptions

The guide describes the current versioned API but is not an OpenAPI schema. API
behavior changes must update the guide and skill together. The skill does not grant
an agent authorization to start a large transfer or another operational mutation.
