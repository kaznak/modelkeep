# ADR-0003: Do not implement the Xet/CAS protocol in ModelKeep

- Status: Accepted
- Date: 2026-08-21

## Context

Current Hugging Face downloads may use Xet/CAS infrastructure. Reimplementing that protocol would substantially increase coupling to upstream internals and maintenance burden. ModelKeep's client-facing requirement is to provide archived model bytes, not to reproduce Hugging Face's storage backend.

## Decision

ModelKeep will not implement Xet/CAS itself.

For upstream acquisition, delegate to the official supported Hugging Face client/fetcher where necessary. Once materialized in the archive, ModelKeep serves the original file bytes to clients using ordinary HTTP semantics, including Range support where required.

ModelKeep must not intentionally redirect client payload downloads around the mirror to upstream Xet endpoints.

## Rationale

This confines Xet compatibility to an upstream adapter maintained by Hugging Face while keeping the durable archive independent of Xet cache formats and protocol changes.

## Alternatives considered

- Native Xet implementation: rejected due to complexity and unnecessary coupling.
- Let GX10 follow Xet redirects directly: rejected because it defeats pull-through persistence and allows accidental bypass of QNAP.

## Consequences

The runtime image may initially include an official HF client/Python closure, making it larger than a pure Rust image. This is acceptable until a simpler upstream adapter can be proven compatible.

## Validation

A cold miss can be acquired from current Hugging Face infrastructure, while a subsequent client download succeeds with upstream connectivity blocked.
