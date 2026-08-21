# ModelKeep

<p align="right"><a href="README.md">English</a> · <a href="README.ja.md">日本語</a></p>

ModelKeep is a persistent pull-through mirror for Hugging Face model repositories.
It stores archived revisions as ordinary files on durable storage and exposes an HTTP
endpoint that existing `hf` and `huggingface_hub` clients can use through `HF_ENDPOINT`.

Architecture, milestones, and requirement traceability are maintained in the
[development plan](docs/development-plan.md).

The QNAP archive is the durable source of truth. Client caches, indexes, server
processes, and container images are replaceable and must not require archived model
data to be downloaded again.

## MVP status

The current MVP provides:

- immutable commit-based revisions and separate mutable refs;
- atomic publication, crash recovery, and SHA-256 manifests;
- HTTP `GET`, `HEAD`, byte ranges, conditional requests, and Hub model metadata;
- single-flight pull-through acquisition through the official Hugging Face client;
- import of existing Hugging Face Hub caches;
- structured operational logging;
- reproducible Nix packages and amd64/arm64 OCI images.

ModelKeep does not implement the Xet/CAS protocol. Upstream acquisition is delegated
to the official Hugging Face client. ModelKeep serves ordinary HTTP files and does not
redirect clients around the mirror.

## Quick start

Point a supported Hugging Face client at ModelKeep:

```sh
export HF_ENDPOINT=http://modelkeep:8090
export HF_HUB_DISABLE_XET=1
hf download Qwen/example-model
```

Archived revisions are served without contacting Hugging Face. A request for a missing
ref or file can trigger an upstream fetch when the official fetch helper is configured.

## QNAP deployment

[`compose.yaml`](compose.yaml) contains the Container Station deployment definition.
The GitHub Actions workflow builds both architectures from the Nix flake and publishes
a multi-architecture image to GHCR when a `v*` tag is pushed:

```sh
git tag v0.2.0
git push origin v0.2.0
```

On QNAP:

```sh
export MODELKEEP_IMAGE_REPOSITORY=ghcr.io/OWNER/REPOSITORY
export MODELKEEP_IMAGE_TAG=0.2.0

docker login ghcr.io
docker compose pull
docker compose up -d
```

Change `/share/LLM/modelkeep` in `compose.yaml` if the QNAP archive share uses another
path. The container runs as UID/GID `10001:10001`, uses a read-only root filesystem,
drops Linux capabilities, writes durable state only under `/data`, and publishes its
HTTP port only on QNAP host loopback. Configure the host's official Tailscale app to
provide the tailnet-only HTTPS endpoint; do not expose port 8090 directly on the LAN.
See [`docs/deployment/qnap-permissions.md`](docs/deployment/qnap-permissions.md) for the host-side UID/GID and permission preflight.
See [`docs/deployment/qnap-tailscale-serve.md`](docs/deployment/qnap-tailscale-serve.md)
for Tailscale Serve setup and boundary checks.

For private or gated upstream repositories, provide `HF_TOKEN` through the deployment
environment. Never put credentials in URLs, manifests, or logs.

## Development

Nix and a Linux execution environment are the only host prerequisites. Enter the
pinned development environment before running Rust or Hugging Face tooling:

```sh
nix develop
```

Run the standard checks inside that environment:

```sh
nix develop -c cargo fmt --check
nix develop -c cargo clippy --all-targets --all-features -- -D warnings
nix develop -c cargo test --all-features
nix flake check
```

Cargo, Rustc, Rustfmt, Clippy, Python, the `hf` client, Git, and CA certificates all
come from the revision pinned by `flake.lock`. The flake exposes formatting, Clippy,
unit-test, and Hugging Face client-environment checks separately. Tests that contact
the real Hugging Face service still require network access.

Build the package or OCI image from the canonical Nix definition:

```sh
nix build .#packages.x86_64-linux.modelkeep
nix build .#packages.x86_64-linux.modelkeep-image
```

Pull requests and pushes to `main` build amd64 and arm64 images without publishing
them. Only `v*` tags push images to GHCR and create the multi-architecture manifest.

## CLI

```sh
modelkeep serve [archive-root] [bind-address]
modelkeep health
modelkeep ready
modelkeep audit [archive-root]
modelkeep refresh [archive-root] <repo-id> <ref> [--dry-run]
modelkeep list [archive-root] <repo-id>
modelkeep show [archive-root] <repo-id> <commit>
modelkeep verify [archive-root] <repo-id> <commit>
modelkeep remove [archive-root] <repo-id> <commit> [--dry-run]
modelkeep import-hf-cache <cache-path> [archive-root]
```

Import and verify an existing client cache before deleting it from a compute node:

```sh
modelkeep import-hf-cache ~/.cache/huggingface/hub /data
modelkeep verify /data Qwen/ExampleModel <commit>
```

The `GET /healthz` endpoint is a lightweight process liveness check. `GET /readyz` verifies that the archive paths are available and writable. The QNAP Compose deployment uses `modelkeep ready` for its container healthcheck.

## Durable archive

```text
/data/
  models/<namespace>/<name>/
    revisions/<commit>/       ordinary model files and manifest
    refs/main                  mutable ref pointing to a commit
  tmp/                         incomplete work only
```

Published revisions are immutable. Moving `main` adds or selects another commit and
does not remove the old revision. Downloads are staged, verified, flushed, and
atomically published; incomplete staging data is never served as a completed object.
There is no automatic archive garbage collection.

The durable design decisions are documented in [`docs/adr/`](docs/adr/), including
ordinary files as the archive representation, immutable revisions, the Xet boundary,
no automatic GC, official upstream acquisition, and derived metadata.

## License

ModelKeep is licensed under the [MIT License](LICENSE).
