# ADR-0002: Archive revisions are immutable; mutable refs are separate

- Status: Accepted
- Date: 2026-08-21

## Context

Hugging Face requests may use mutable names such as `main`, while long-term preservation requires a stable identity. Updating `main` must not silently replace the bytes previously archived under that name.

## Decision

Resolve mutable requested revisions to immutable commit identifiers before publication. Store completed data under the resolved commit ID. Represent `main`, tags, and other mutable refs separately as mappings to commits.

Never modify an already published commit revision in place.

## Rationale

This preserves reproducibility and allows old revisions to remain usable even when upstream refs move or disappear.

## Alternatives considered

- Store only the latest state of `main`: rejected because it destroys historical data.
- Copy files into a mutable `main/` directory: rejected because identity and provenance become ambiguous.

## Consequences

Multiple revisions may consume additional space. Ref updates are cheap metadata operations. Clients requesting an explicit archived commit can be served without upstream access.

## Validation

After `main` moves from commit A to B, both A and B remain retrievable and `main` resolves to B.
