# Hugging Face client compatibility policy

ModelKeep supports the following explicitly pinned `huggingface_hub` client
versions:

| Version | Role |
|---|---|
| 0.36.0 | oldest supported pre-1.0 client |
| 1.27.0 | current client shipped in the pinned nixpkgs input |

Both versions run the same deterministic black-box suite. The suite covers a cold
miss, warm and upstream-offline downloads, `HEAD`, byte `Range`, concurrent warm
downloads, upstream error mapping, and rejection of attempts to bypass ModelKeep.
It uses a local upstream fixture and does not contact Hugging Face. Optional online
protocol observations are deliberately separate from this compatibility gate.

The latest dated upstream matrix and its reproducible, credential-free procedure
are recorded in
[`observations/hugging-face-protocol-2026-09-22.md`](observations/hugging-face-protocol-2026-09-22.md).
Its sanitized [machine-readable record](observations/hugging-face-protocol-2026-09-22.json)
is validated offline by `nix flake check`.
The deterministic suite mirrors its public, safetensors, sharded, immutable
revision, `HEAD`, Range, redirect, and Xet-boundary behaviors without contacting
Hugging Face.

## Updating the matrix

1. Choose versions with a concrete operator need. Keep one oldest-supported version
   and the version used by the production image; do not grow an unbounded historical
   matrix.
2. Update `supportedHfClientsFor` in `flake.nix`. Pin non-nixpkgs releases with the
   PyPI source archive SHA-256. The current-client key must also be updated when the
   pinned nixpkgs package changes.
3. Update the table above and run:

   ```sh
   nix flake check
   ```

4. Confirm the `Build amd64 image` and `Build arm64 image` GitHub jobs pass. Each job
   executes the complete flake check natively, so every supported client is tested on
   both architectures.

Removing a version is an explicit support-policy change and must be documented in the
same commit. A new version is supported only after its matrix check passes.
