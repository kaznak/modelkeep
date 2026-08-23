---
status: open
priority: P2
related_adrs:
  - ADR-0015
created: 2026-08-24
updated: 2026-08-24
---
# Issue 0051: Make the management authentication UI match the active mode

- Status: Open
- Priority: P2
- Related ADR: ADR-0015

## Objective

Show the bearer-token prompt only when it is usable and authentication is actually
required, while clearly representing a successful Tailscale-authenticated session.

## Problem

The authentication panel has the HTML `hidden` attribute, but the `.auth` CSS rule
sets `display: flex` and overrides the browser's hidden presentation. As a result,
the management page continues to display **Authentication required** and request a
Bearer token even after Tailscale app-capability authorization succeeds. In the QNAP
configuration `MODELKEEP_ADMIN_TOKEN` is unset, so the requested token cannot satisfy
any configured authentication path.

Bearer authentication is currently enabled implicitly only when a non-empty
`MODELKEEP_ADMIN_TOKEN` is configured. The UI has no authenticated way to discover
which login mechanisms are available.

## Scope

- Fix the hidden-state CSS and add a regression test that proves the authentication
  panel is not rendered visibly before an actual 401 response.
- Let the authenticated management bootstrap/status response report non-secret
  authentication context sufficient for the UI to distinguish bearer and Tailscale
  authorization without revealing token values or accepting client-selected modes.
- On successful Tailscale authorization, hide the bearer form and show the current
  connection/principal state supplied by Issue 0050.
- On a 401 response, offer a bearer-token form only when bearer authentication is
  configured; otherwise show an actionable Tailscale authorization error.
- Preserve session-only browser storage for manually entered bearer tokens and clear
  a rejected token without logging or reflecting it.
- Keep the existing server rule that an absent or empty `MODELKEEP_ADMIN_TOKEN`
  disables bearer authentication. Do not add a redundant boolean switch unless an
  implementation constraint demonstrates a need for one.

## Acceptance criteria

- With Tailscale-only authentication, a successful page load never displays the
  bearer-token form or **Authentication required** message.
- With bearer-only authentication, a 401 displays the token form and a valid token
  reconnects successfully.
- With both methods configured, a request already authorized by Tailscale does not
  prompt for a token; an unauthorized request can use the bearer form.
- With no usable authorization path, the server retains its existing startup failure
  rather than serving a misleading UI.
- CSS/DOM tests cover the `hidden` regression, and API/UI tests cover Tailscale-only,
  bearer-only, combined, rejected-token, and unauthorized-Tailscale states.

## Verification

```sh
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
nix flake check
```

Verify the packaged UI through the QNAP `svc:modelkeep-admin` hostname with the
production Tailscale-only Compose configuration, then exercise bearer-only mode in an
isolated local deployment.

## Risks and assumptions

An unauthenticated endpoint must not disclose unnecessary deployment or policy
details merely to customize the login screen. The implementation may need to encode
coarse authentication hints in a 401 response or render them into the same-origin UI;
the selected mechanism must not weaken authorization or expose secrets.
