# ModelKeep test coverage strategy

ModelKeep tests are organized by trust boundary. Passing a unit test on each side of
a boundary is not equivalent to exercising the composed production path. Every
release-critical behavior therefore needs one owning black-box check that uses the
same producer and consumer shipped in the image.

## Coverage matrix

| Behavior | Focused coverage | Owning black-box/Nix check | QNAP-only evidence |
|---|---|---|---|
| Archive paths, manifests, integrity, immutable publication | Rust unit tests | `checks.tests`, `archive-audit-cli` | Filesystem capacity and ACL behavior |
| Helper event parsing and safe failures | Rust parser tests | `hf-fetcher-tests` plus production helper in supported-client checks | None |
| Cold model and dataset acquisition | HTTP/archive unit tests | `hf-client-integration-0-36` and `hf-client-integration-1-27` | Upstream bandwidth and gated credentials |
| Warm and upstream-offline downloads | HTTP/archive unit tests | supported-client integration and `archive-restore-drill` | Tailnet and storage availability |
| Legacy durable metadata | manifest validation unit tests | supported-client integration injects legacy internal entries and downloads with a fresh client | Existing site archive diversity |
| Crash before publication | staging and single-flight unit tests | `archive-crash-upgrade` sends SIGKILL and proves no partial revision is served | Container Station process behavior |
| Resume after restart | lease/adoption unit tests and helper tests | `archive-crash-upgrade` reuses retained partial state through `hf_fetch.acquire`, publishes, and removes staging | QNAP filesystem timing and retained capacity |
| Progress semantics | helper counter tests and Admin job tests | `hf-fetcher-tests` exercises incomplete-byte movement and rejects unchanged heartbeat events | UI observation during a representative large transfer |
| Released-writer/current-reader compatibility | manifest and reconstruction unit tests | `archive-crash-upgrade` reads an archive created by the pinned old release | Representative long-lived site archive |

## Boundary ownership rules

1. A synthetic helper is suitable for fault injection, but cannot be the sole test of
   the helper/parser contract. At least one owning check must execute the tracked
   production helper implementation.
2. A mocked client is suitable for deterministic helper unit tests, but cannot be the
   sole compatibility evidence. Every supported `huggingface_hub` version runs as a
   real client against a deterministic local ModelKeep deployment.
3. A fixture that independently reimplements both sides of an assumed contract does
   not prove that contract. Prefer producing state with one shipped component and
   consuming it with another.
4. Crash checks must observe a durable checkpoint before killing the process. A sleep
   alone is not evidence that the intended state was reached.
5. Progress means a counter changed. A phase heartbeat or repeated identical counter
   must not refresh the user-visible transfer-progress timestamp.
6. QNAP acceptance validates filesystem, Container Station, networking, and operator
   configuration. Deterministic application behavior belongs in CI and must not be
   deferred to the QNAP drill.

## Regression ownership

When a released defect is found, add the regression at the highest boundary that can
reproduce it deterministically, then retain focused unit coverage for diagnosis. In
particular:

- internal staging entries visible to clients are owned by the supported real-client
  integration check using legacy durable metadata;
- restart/resume parser failures are owned by `archive-crash-upgrade` through the
  production helper boundary;
- incomplete-byte accounting and false progress heartbeats are owned by
  `hf-fetcher-tests`, with Admin state transition coverage in Rust;
- architecture-specific packaging remains covered by the native amd64 and arm64
  image jobs rather than inferred from an x86-only unit run.

An issue may remain open for a QNAP observation after deterministic CI passes, but the
observation should confirm runtime-specific behavior only. It must not be the first
test of an ordinary application control flow.
