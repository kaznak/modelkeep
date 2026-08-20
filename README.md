# ModelKeep

<p align="right"><a href="README.md">English</a> · <a href="README.ja.md">日本語</a></p>

ModelKeep is a persistent pull-through mirror for Hugging Face model repositories.
It stores archived revisions as ordinary files on durable storage and exposes an HTTP
endpoint that existing `hf` and `huggingface_hub` clients can use through `HF_ENDPOINT`.

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
git tag v0.1.0
git push origin v0.1.0
```

On QNAP:

```sh
export MODELKEEP_IMAGE_REPOSITORY=ghcr.io/OWNER/REPOSITORY
export MODELKEEP_IMAGE_TAG=0.1.0

docker login ghcr.io
docker compose pull
docker compose up -d
```

Change `/share/LLM/modelkeep` in `compose.yaml` if the QNAP archive share uses another
path. The container runs as UID/GID `10001:10001`, uses a read-only root filesystem,
drops Linux capabilities, and writes durable state only under `/data`.

For private or gated upstream repositories, provide `HF_TOKEN` through the deployment
environment. Never put credentials in URLs, manifests, or logs.

## Development

Run the standard checks locally:

```sh
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
nix flake check
```

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
modelkeep list [archive-root] <repo-id>
modelkeep show [archive-root] <repo-id> <commit>
modelkeep verify [archive-root] <repo-id> <commit>
modelkeep import-hf-cache <cache-path> [archive-root]
```

Import and verify an existing client cache before deleting it from a compute node:

```sh
modelkeep import-hf-cache ~/.cache/huggingface/hub /data
modelkeep verify /data Qwen/ExampleModel <commit>
```

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
