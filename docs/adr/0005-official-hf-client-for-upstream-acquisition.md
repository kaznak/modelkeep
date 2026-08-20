# ADR-0005: Prefer the official Hugging Face client for upstream acquisition

- Status: Accepted
- Date: 2026-08-21

## Context

ModelKeep needs robust upstream downloads across Hugging Face protocol changes, including Xet-related behavior, while its client-facing server should remain small and controlled.

## Decision

For the initial implementation, use the official supported Hugging Face client/fetcher as the upstream acquisition mechanism, isolated behind a fetch-worker interface.

The Rust HTTP server owns mirror policy, archive layout, atomic publication, validation, concurrency control, and client serving. The upstream fetcher is replaceable.

## Rationale

This minimizes protocol reimplementation and lets Hugging Face's supported client absorb upstream transport changes. Isolation prevents the official client's cache representation from becoming ModelKeep's durable archive format.

## Alternatives considered

- Implement all upstream HTTP/Xet behavior directly in Rust: deferred until there is evidence that doing so materially improves reliability or deployment.
- Shell out to arbitrary user tooling: rejected as an uncontrolled compatibility boundary.

## Consequences

The first OCI image may not be as small as a Rust-only image. The worker boundary must clearly define staging output, resolved revision, provenance, and errors.

## Validation

Integration tests exercise cold acquisition through the worker and warm/offline serving without the worker contacting upstream.
