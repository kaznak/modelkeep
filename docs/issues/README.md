# ModelKeep issues

This directory is the repository-local issue tracker for ModelKeep.

Until a separate project-management workflow is adopted, active work is tracked here
instead of GitHub Issues. Each issue is a small, reviewable unit of work. Durable
architecture decisions belong in [`docs/adr/`](../adr/), not only in an issue file.

## Naming and status

Issue files use a zero-padded number and a short kebab-case title:

```text
NNNN-short-description.md
```

[`LAST_ISSUE_NUMBER`](LAST_ISSUE_NUMBER) records the highest issue number ever
assigned, including completed issues whose files have been removed. When creating an
issue, assign the next number and update that file in the same commit. Never reuse an
issue number.

Each issue must begin with YAML frontmatter containing `status`, `priority`, `related_adrs`, `created`, and `updated`. The body should also state:

- `Status`: `Open`, `In Progress`, `Blocked`, or `Done`;
- `Priority`: `P0` through `P3`;
- `Related ADR`: applicable decisions, or `None`;
- objective and problem statement;
- explicit acceptance criteria;
- exact verification commands or scenarios;
- material risks and assumptions.

Priority meanings:

- `P0`: correctness or data-safety issue that blocks reliable use;
- `P1`: important for normal operation but not an immediate data-safety blocker;
- `P2`: useful operational or compatibility improvement;
- `P3`: deferred enhancement.

## Current backlog

- [0020 — Add an archive integrity audit workflow](0020-add-archive-integrity-audit-workflow.md) (P2)
- [0004 — Add storage observability](0004-add-storage-observability.md) (P3)
- [0005 — Define the client network boundary](0005-define-client-network-boundary.md) (P3)
