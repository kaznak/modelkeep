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

- [0022 — Complete the protocol observation matrix](0022-complete-protocol-observation-matrix.md) (P1)
- [0023 — Test multiple supported Hugging Face client versions](0023-test-multiple-hf-client-versions.md) (P1)
- [0024 — Add black-box crash and upgrade tests](0024-add-black-box-crash-and-upgrade-tests.md) (P1)
- [0025 — Validate large Hugging Face cache migration](0025-validate-large-hf-cache-migration.md) (P1)
- [0026 — Complete QNAP and GX10 acceptance testing](0026-complete-qnap-gx10-acceptance-testing.md) (P1)
- [0030 — Define the management control-plane contract](0030-add-management-api.md) (P1)
- [0045 — Add the read-only management API](0045-add-read-only-management-api.md) (P1)
- [0046 — Add asynchronous archive jobs](0046-add-asynchronous-archive-jobs.md) (P1)
- [0047 — Add the QNAP management web UI](0047-add-qnap-management-web-ui.md) (P1)
- [0027 — Complete structured operational events](0027-complete-structured-operational-events.md) (P2)
- [0048 — Add the remote revision deletion workflow](0048-add-remote-revision-deletion-workflow.md) (P2)
- [0021 — Add tailnet identity-aware authorization](0021-add-tailnet-identity-aware-authorization.md) (P3)
- [0004 — Add storage observability](0004-add-storage-observability.md) (P3)
- [0028 — Define private and gated repository credential policy](0028-define-private-gated-credential-policy.md) (P3)
- [0029 — Add dataset repository support](0029-add-dataset-repository-support.md) (P3)
- [0031 — Define repository pinning policy](0031-define-repository-pinning-policy.md) (P3)
- [0032 — Add multi-QNAP replication](0032-add-multi-qnap-replication.md) (P3)
- [0033 — Add an S3 storage backend](0033-add-s3-storage-backend.md) (P3)
- [0034 — Add an OCI model registry backend](0034-add-oci-model-registry-backend.md) (P3)
- [0035 — Add archive export and import interoperability](0035-add-archive-interoperability.md) (P3)
- [0036 — Add non-Hugging-Face upstream sources](0036-add-non-hugging-face-upstreams.md) (P3)
