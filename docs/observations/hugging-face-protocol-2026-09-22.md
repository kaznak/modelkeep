# Hugging Face protocol observation — 2026-09-22 UTC

This is an upstream observation, not a ModelKeep policy definition. The stable
ModelKeep contract is enforced by the deterministic black-box suite described
below. Upstream behavior can change and should be re-observed before changing the
compatibility surface.

The sanitized machine-readable output from this run is preserved alongside this
record as
[`hugging-face-protocol-2026-09-22.json`](hugging-face-protocol-2026-09-22.json).
An offline flake check validates its schema, date, complete matrix, pinned repository
constants, and absence of URL query strings or credential-bearing fields.

## Environment and fixtures

- `huggingface_hub`: `1.27.0`, the current client pinned by `flake.nix`
- Authentication: none (`token=False`); public repositories only
- Public/safetensors representative:
  `optimum-intel-internal-testing/tiny-random-bert` at
  `8301186e0724d418a8d4f5eb17011c9226f5b766`
- Sharded representative:
  `bumblebee-testing/tiny-random-GPT2Model-sharded` at
  `4fca22a84867aacca5dcf7317144782ea1807e1a`
- Downloaded payload: less than 1 MiB in total. No model payload is committed.

The observer records methods, hosts, paths, status codes, `Range`,
`Accept-Ranges`, `Content-Range`, and `x-repo-commit`. It deliberately omits all
query strings, authorization/cookie headers, and full redirect URLs. A redirect
destination is reduced to its host name.

## Matrix

| Category | Observed supported-client behavior | Deterministic ModelKeep regression |
|---|---|---|
| Small public model | A commit-pinned model-info `GET` returned the requested 40-character commit; a commit-pinned `config.json` completed through `HEAD` then `GET`. | A real client performs cold and warm/offline snapshot downloads and verifies `config.json`. |
| Safetensors | `model.safetensors` was reported as 520,212 bytes with SHA-256/ETag `965f02b6a7e5520fc12f710e4e3b6132f697f1c8f648819553c5ade86752d2de`; the downloaded digest matched. | The fixture publishes `model.safetensors`; cold and offline clients verify its exact bytes. |
| Sharded model | The tree listed an index plus three safetensors shards. The official client downloaded the index and all three pinned shards (337,852 payload bytes total). | The fixture publishes an index plus two shards; cold and offline clients verify every shard byte-for-byte. |
| Revision-specific | Both API and resolve/tree paths used the requested immutable commit, and resolve responses returned the same `x-repo-commit`. | Downloads by mutable `main` resolve to the fixture commit; subsequent offline download explicitly uses that commit. |
| `HEAD` | The client issued `HEAD` to `/resolve/<commit>/...`; small regular files followed a `307` to the Hub resolve cache, while the safetensors file returned `302`. Successful responses included `Accept-Ranges: bytes` and `x-repo-commit`. | A direct `HEAD` against ModelKeep verifies status 200, exact `Content-Length`, and an empty body. |
| Range | `GET` with `Range: bytes=0-0` returned `302`, then the redirected public payload host returned `206` with `Content-Range: bytes 0-0/520212`. | A direct black-box HTTP request verifies ModelKeep's Range server contract (`206` and exact bytes). This is intentionally separate from supported-client behavior because the official client does not reliably issue Range requests for a completed fresh download. |
| Redirect | Upstream used a Hub-internal `307` for a regular file and a `302` to `us.aws.cdn.hf.co` for the Xet-backed file. Signed redirect query data was not retained. | The client is confined to localhost during the complete suite. ModelKeep's payload response is asserted to contain no `Location` header. |
| Xet-backed file | `get_hf_file_metadata` returned `xet_file_data` for the safetensors file. The official client materialized it and its digest matched the API metadata. | Xet remains upstream-only: the local fixture is served as ordinary bytes and responses contain neither `x-xet-hash` nor `x-linked-etag`. |

The observed upstream redirect is evidence for why ModelKeep must terminate the
download itself. It is not copied into ModelKeep responses. This preserves
ADR-0003: a GX10 client cannot silently leave the mirror for a CDN/Xet payload.

## Reproduction

Run from a clean checkout. The command uses only pinned public revisions and
refuses to run if a common Hugging Face token environment variable is present:

```sh
env -u HF_TOKEN -u HUGGING_FACE_HUB_TOKEN \
  nix run .#hf-protocol-observation -- \
  --output /tmp/modelkeep-hf-protocol.json
```

Confirm every value under `matrix` is `true`, review changes in methods/paths and
selected response headers, then add the sanitized JSON and a new dated Markdown
record together. Do not overwrite an earlier record: observations are historical
evidence. The observer replaces failures with a fixed safe message and the offline
validator rejects query strings or credential-bearing fields. Never commit the
download cache or an unsanitized transport trace because upstream redirects can
carry short-lived signed data.

The online command is intentionally not a CI gate. The deterministic gate runs
the same ModelKeep behavior with every client listed in
[`hugging-face-client-compatibility.md`](../hugging-face-client-compatibility.md):

```sh
nix build .#checks.x86_64-linux.hf-client-integration-0-36
nix build .#checks.x86_64-linux.hf-client-integration-1-27
```

The integration test also inspects ModelKeep's structured request events to prove
that each supported official client issued a `HEAD` for `model.safetensors`. Its
separate direct Range request validates ModelKeep's HTTP compatibility surface; it
is not presented as a request that every official-client version necessarily emits.

The observation procedure uses the official public APIs documented for
[`hf_hub_download`, `get_hf_file_metadata`, and file metadata](https://huggingface.co/docs/huggingface_hub/package_reference/file_download).
The official [`HF_DEBUG` documentation](https://huggingface.co/docs/huggingface_hub/package_reference/environment_variables#hf-debug)
is useful for interactive diagnosis, but its raw equivalent-cURL output is not
stored because it can include headers or signed URLs.
