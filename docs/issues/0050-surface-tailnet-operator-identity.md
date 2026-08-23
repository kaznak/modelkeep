---
status: open
priority: P2
related_adrs:
  - ADR-0015
created: 2026-08-24
updated: 2026-08-24
---
# Issue 0050: Surface the authenticated tailnet operator identity

- Status: Open
- Priority: P2
- Related ADR: ADR-0015

## Objective

Show and audit the Tailscale user associated with a management request without using
display identity as the authorization decision.

## Problem

Tailscale Serve automatically forwards `Tailscale-User-Login`,
`Tailscale-User-Name`, and optionally `Tailscale-User-Profile-Pic` for user-owned
source devices. ModelKeep currently ignores those headers, so an operator authorized
through `io.modelkeep/cap/admin` cannot see which identity is active and management
jobs do not record who initiated them.

## Scope

- Read Tailscale identity headers only when trusted Tailscale-header mode is enabled
  on the loopback-only management listener.
- Add a small authenticated API representation of the current principal and expose
  the login and display name in the management UI.
- Record the initiating principal on management jobs and credential-safe audit events
  where practical; do not place profile image URLs or unnecessary personal data in
  durable operational state.
- Represent requests from tagged devices and requests authorized by bearer token
  without inventing a user identity.
- Continue to authorize Tailscale requests exclusively with
  `io.modelkeep/cap/admin`; identity headers are display and audit inputs, not proof
  of administrative permission.
- Document the privacy, retention, trusted-proxy, and tagged-device behavior.

## Acceptance criteria

- An authorized user-owned tailnet client sees its Tailscale login and display name
  in the management UI.
- A tagged source device receives a clear non-user principal representation and is
  not rejected solely because user identity headers are absent.
- A bearer-authorized request is represented without being misidentified as a
  Tailscale user.
- Identity headers cannot authorize a request without the required app capability or
  valid bearer token.
- Identity headers are ignored when trusted Tailscale-header mode is disabled.
- Tests cover user identity, tagged/no-identity traffic, bearer authentication,
  malformed/non-ASCII header handling, and forged headers outside the trusted mode.

## Verification

```sh
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
nix flake check
```

On QNAP, access the admin Service from a user-owned device and confirm the displayed
login matches the identity known to Tailscale. Repeat from a permitted tagged device
if available and confirm that no user identity is fabricated.

## Risks and assumptions

Tailscale identity headers are ordinary trusted-proxy headers rather than signed
tokens, so this change must not broaden the existing loopback Serve trust boundary.
Login and display names are personal data and should be retained only where they add
operational accountability. This issue does not implement the per-repository
identity-aware authorization tracked by Issue 0021.
