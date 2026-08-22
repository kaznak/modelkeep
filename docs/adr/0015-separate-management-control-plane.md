# ADR-0015: Separate and explicitly authorize the management control plane

- Status: Accepted
- Date: 2026-08-22

## Context

ModelKeep's Hugging Face-compatible routes are a download data plane. Operators also
need browser-based inventory, prefetch, refresh, verification, and audit without SSH.
Putting administrative mutations on the same listener and relying only on tailnet
membership would let ordinary model clients discover control routes and would leave
browser requests exposed to cross-site request forgery.

Tailscale Serve can expose named Services with distinct grants. Tailscale v1.92 and
later can also forward selected application capabilities in the
`Tailscale-App-Capabilities` header. Deployments that cannot use that feature still
need a narrow explicit credential option.

## Decision

The management control plane is an optional second HTTP listener, separate from the
Hugging Face-compatible listener. It binds to host loopback in the supported Compose
deployment and is published, when enabled, through a distinct Tailscale Service such
as `svc:modelkeep-admin`. Ordinary download ingress never proxies to this port.

Management requests require one of these explicitly configured authorization modes:

1. a configured bearer token matched without logging it; or
2. a trusted Tailscale Serve `Tailscale-App-Capabilities` header containing
   `io.modelkeep/cap/admin`.

Tailscale capability headers are accepted only when the operator explicitly enables
header trust and the management listener is loopback-only. Direct non-loopback
management binding is rejected in that mode. Tagged devices and users are both
supported through app capabilities; identity display is not an authorization input.

State-changing requests additionally require `X-ModelKeep-CSRF: 1`. ModelKeep emits
no permissive CORS headers and uses no ambient application session cookie. The
replaceable same-origin UI may hold a bearer token only in browser session memory.

The versioned API separates:

- read-only inventory and service state;
- asynchronous prefetch, refresh, verify, and audit jobs;
- destructive revision deletion, which requires a later separately authorized
  dry-run and confirmation workflow under ADR-0007.

Job records are operational metadata. They may be persisted atomically for restart
diagnostics, but archived files and manifests remain the authority. Interrupted
running jobs become explicit interrupted failures after restart; they never imply a
completed publication.

## Rationale

A separate listener makes the data-plane/control-plane boundary testable without
depending on path filtering in Tailscale. App capabilities provide tailnet identity
authorization without inventing a second user database. The bearer mode supports
older QNAP Tailscale packages and non-Tailscale deployments. Explicit CSRF headers
protect mutations even when a browser can reach the tailnet hostname.

## Alternatives considered

- Tailnet membership alone: rejected because download permission does not imply
  administrative permission and default tailnet policies may allow all members.
- Management paths on port 8090: rejected because the ordinary download Service
  would also reach them.
- Trust identity/capability headers on a LAN-bound listener: rejected because clients
  could forge proxy headers.
- Application user/password database: deferred because it adds identity and password
  lifecycle state that Tailscale already provides.
- Synchronous acquisition endpoints: rejected because multi-gigabyte operations
  outlive normal request and browser timeouts.

## Consequences

Operators configure a second loopback port and Tailscale Service. Tailscale app-header
authorization requires Serve v1.92 or later and `--accept-app-caps`; older deployments
use a bearer token. The UI remains optional and removable. Management metadata can be
discarded and rebuilt without redownloading archived revisions.

## Validation

Black-box tests must prove listener separation, disabled-by-default behavior, bearer
and capability authorization, rejection of forged/untrusted headers, CSRF rejection,
no permissive CORS, credential-safe logs, and unchanged real Hugging Face client
behavior on the data listener.
