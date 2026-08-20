# Architecture Decision Records

ADRs record durable architectural decisions for ModelKeep.

- `Accepted` decisions govern implementation until superseded.
- Do not rewrite accepted history to reflect a new decision.
- Create a new ADR, mark the old ADR `Superseded by ADR-NNNN`, and explain the migration/compatibility consequences.
- Use `0000-template.md` for new records.

Initial records:

- ADR-0001 — ordinary materialized files as the durable archive
- ADR-0002 — immutable revisions and separate mutable refs
- ADR-0003 — no Xet/CAS implementation in ModelKeep
- ADR-0004 — no automatic archive GC
- ADR-0005 — official HF client for upstream acquisition
- ADR-0006 — metadata indexes are derived state
