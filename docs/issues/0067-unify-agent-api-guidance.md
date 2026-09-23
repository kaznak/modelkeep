---
status: in-progress
priority: P2
related_adrs:
  - ADR-0015
created: 2026-09-23
updated: 2026-09-23
---
# Issue 0067: Unify agent guidance for ModelKeep APIs

- Status: In Progress
- Priority: P2
- Related ADR: ADR-0015

## Objective

Provide one portable `modelkeep` skill that directs agents to the supported
Hugging Face-compatible download interface or the Admin API according to the task.

## Problem

The repository documents and packages agent guidance for the Admin API, but the
skill does not explain normal model and dataset access through `hf` or
`huggingface_hub`. An agent can therefore discover the management workflow without
discovering the primary download interface or the separation between the download
and management origins.

## Write scope

- a data-plane API and client guide;
- the repository-local ModelKeep skill;
- README links and development-plan traceability.

## Do not touch

- HTTP behavior, authentication policy, archive representation, or deployment
  configuration;
- site-specific endpoints, identities, credentials, or inventory;
- archive deletion or deployment operations.

## Acceptance criteria

- One `modelkeep` skill covers both ordinary model/dataset retrieval and Admin API
  operations, and routes each task to the correct interface.
- The guide prefers supported `hf` and `huggingface_hub` clients for downloads and
  documents the implemented model and dataset compatibility routes.
- Download and management origins come only from user-supplied environment variables
  or ignored site configuration and are never conflated.
- The guidance explains that a cold download can create durable archive content and
  that explicit management mutations retain their existing authorization rules.
- No deployment-specific URL, identity, credential, or private job data is added.

## Verification

```sh
nix develop -c python3 /home/agent/.codex/skills/.system/skill-creator/scripts/quick_validate.py .agents/skills/modelkeep
git diff --check
rg -n 'modelkeep-admin skill|skills/modelkeep-admin' README.md README.ja.md docs .agents
```

Review the guide against the registered routes and supported HF client integration
tests.

## Risks and assumptions

The compatibility surface is intentionally a supported subset of the Hub API. The
guide must not imply support for upload, arbitrary Hub endpoints, or direct upstream
redirects. A normal client read can cause a cold pull-through acquisition, so it is
not always operationally read-only even though it uses `GET` and `HEAD`.
