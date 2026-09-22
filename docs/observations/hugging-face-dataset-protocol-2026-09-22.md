# Hugging Face dataset protocol observation — 2026-09-22

This is a credential-free observation of the public Hugging Face Hub using
`huggingface_hub` 1.27.0. It records only stable paths and status codes. Query
strings, request headers, cookies, and redirect targets are deliberately not
recorded.

## Fixed fixture

- Repository type: `dataset`
- Repository: `lhoestq/demo1`
- Immutable revision: `87ecf163bedca9d80598b528940a9c4f99e14c11`
- Files reported by the API: `.gitattributes`, `README.md`, `data/test.csv`,
  and `data/train.csv`
- Download selection: `data/test.csv`
- Authentication: explicitly disabled with `token=False`; no `HF_TOKEN` or
  `HUGGING_FACE_HUB_TOKEN` was supplied

The following official-client operations succeeded against the immutable
revision:

```python
HfApi(token=False).repo_info(
    "lhoestq/demo1",
    repo_type="dataset",
    revision="87ecf163bedca9d80598b528940a9c4f99e14c11",
    files_metadata=True,
    token=False,
)

snapshot_download(
    "lhoestq/demo1",
    repo_type="dataset",
    revision="87ecf163bedca9d80598b528940a9c4f99e14c11",
    allow_patterns=["data/test.csv"],
    token=False,
)
```

Reproduce the safe machine-readable observation without exporting a Hugging
Face token:

```sh
nix run .#hf-dataset-protocol-observation -- \
  --output /tmp/hugging-face-dataset-protocol.json
```

Compare the result with the committed sanitized record
`hugging-face-dataset-protocol-2026-09-22.json`. The observer refuses to run
when `HF_TOKEN` or `HUGGING_FACE_HUB_TOKEN` is exported.

## Observed request surface

| Method | Safe path | Status | Purpose |
| --- | --- | --- | --- |
| `GET` | `/api/datasets/lhoestq/demo1/revision/<commit>` | `200` | repository metadata and immutable revision |
| `GET` | `/api/datasets/lhoestq/demo1/tree/<commit>` | `200` | snapshot file inventory |
| `HEAD` | `/datasets/lhoestq/demo1/resolve/<commit>/data/test.csv` | `307` | file metadata at the public Hub |
| `HEAD` | `/api/resolve-cache/datasets/lhoestq/demo1/<commit>/data/test.csv` | `200` | redirected public-Hub metadata request |
| `GET` | `/api/resolve-cache/datasets/lhoestq/demo1/<commit>/data/test.csv` | `200` | redirected public-Hub payload request |

The public Hub's first redirect was relative, but the full `Location` value was
not retained. ModelKeep must not reproduce that upstream redirect: its dataset
resolve endpoint must serve archived bytes itself so a configured client cannot
silently bypass the mirror.

## Compatibility conclusion

Dataset clients use a distinct plural API namespace and a distinct resolve
prefix:

```text
/api/datasets/{namespace}/{repo}/revision/{revision}
/api/datasets/{namespace}/{repo}/tree/{revision}
/datasets/{namespace}/{repo}/resolve/{revision}/{path}
```

The corresponding model routes omit the `/datasets` resolve prefix and use
`/api/models`. Consequently, repository type must be carried through routing,
upstream acquisition, durable identity, and manifest metadata; deriving it
only from `{namespace}/{repo}` would allow a model and dataset with the same ID
to collide.

The checked-in integration suite exercises these routes with both supported
client versions (0.36.0 and 1.27.0), including cold acquisition, warm/offline
downloads, HEAD, Range, no-redirect/no-bypass behavior, and a same-ID
model/dataset collision case. The local fixture is deterministic; this online
observation is evidence for the route shape rather than a CI dependency.
