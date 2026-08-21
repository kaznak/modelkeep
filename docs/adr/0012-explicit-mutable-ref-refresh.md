# ADR-0012: Mutable refs refresh only through an explicit administrative action

- Status: Accepted
- Date: 2026-08-21

## Context

Warm client requests must remain available without upstream access, but operators
also need to archive a newer commit selected by `main` or another mutable ref.

## Decision

Normal HTTP requests never refresh an already archived mutable ref. A dedicated
`modelkeep refresh` administrative command resolves and acquires the upstream ref,
publishes the complete immutable revision, and only then atomically updates the ref.
Dry-run performs acquisition and validation but publishes nothing and changes no ref.

## Rationale

This keeps ordinary reads deterministic and offline-capable while making upstream
state changes explicit and auditable.

## Alternatives considered

- Refresh on every request: rejected because warm reads would depend on upstream.
- Time-based refresh: rejected because it introduces hidden policy and failure modes.

## Consequences

Operators must schedule refresh explicitly. Dry-run can consume upstream bandwidth
and temporary space because it validates a complete snapshot.

## Validation

Tests prove A remains available after refresh to B, acquisition failure leaves the
ref at A, dry-run publishes nothing, and warm HTTP requests do not invoke upstream.
