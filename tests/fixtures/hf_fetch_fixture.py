#!/usr/bin/env python3
"""Upstream fixture implementing the fetch helper's stdout event contract.

It answers both helper modes: a resolve-only invocation, which reports the
commit and its per-file upstream metadata without transferring anything, and an
acquisition, which materializes the payload. The resolve-only answer is what lets
ModelKeep report a revision it has never archived (Issue 0074).
"""

import argparse
import hashlib
import json
import sys
from pathlib import Path


COMMIT = "a" * 40


def upstream_blob_id(path):
    """The git object id upstream reports for a path.

    A git blob id is a sha1 over the blob, so it is deliberately not any digest
    of the content ModelKeep serves: a check comparing the two catches a `tree`
    response that reports one under the other's name (Issue 0079). The same rule
    is spelled out in `tests/hf_client_integration.py`.
    """
    return hashlib.sha1(b"blob:" + path.encode()).hexdigest()


def repository_files(payloads):
    """Upstream's per-file metadata for the whole commit.

    `blob_id` is reported for every file, and `lfs_sha256` for the LFS-managed
    ones, which is what `repo_info(files_metadata=True)` returns. The LFS digest
    is the real digest of the payload, as upstream's would be.
    """
    entries = []
    for path in sorted(payloads):
        entry = {
            "path": path,
            "size": len(payloads[path]),
            "blob_id": upstream_blob_id(path),
        }
        if path.endswith(".safetensors"):
            entry["lfs_sha256"] = hashlib.sha256(payloads[path]).hexdigest()
        entries.append(entry)
    return entries


def model_payloads():
    return {
        "README.md": b"# ModelKeep model fixture\n",
        "config.json": b'{"model_type":"modelkeep-fixture"}',
        "tokenizer.json": b'{"version":"1.0"}',
        "model.safetensors": b"MODELKEEP-SAFETENSORS-FIXTURE",
        "model-00001-of-00002.safetensors": b"MODELKEEP-SHARD-ONE",
        "model-00002-of-00002.safetensors": b"MODELKEEP-SHARD-TWO",
        "model.safetensors.index.json": json.dumps(
            {
                "metadata": {"total_size": 38},
                "weight_map": {
                    "layer.0": "model-00001-of-00002.safetensors",
                    "layer.1": "model-00002-of-00002.safetensors",
                },
            },
            separators=(",", ":"),
        ).encode(),
    }


def dataset_payloads():
    return {
        "README.md": b"# ModelKeep dataset fixture\n",
        "data/test.csv": b"split,value\ntest,dataset\n",
        "data/train.csv": b"split,value\ntrain,dataset\n",
    }


parser = argparse.ArgumentParser()
parser.add_argument("--repo-id", required=True)
parser.add_argument("--repo-type", choices=("model", "dataset"), default="model")
parser.add_argument("--revision", required=True)
parser.add_argument("--output")
parser.add_argument("--file", action="append")
parser.add_argument("--exclude", action="append")
parser.add_argument("--resolve-only", action="store_true", dest="resolve_only")
args = parser.parse_args()

if args.revision == "missing":
    sys.exit(11)
if args.revision == "private":
    sys.exit(12)
if args.revision == "unavailable":
    sys.exit(10)

payloads = dataset_payloads() if args.repo_type == "dataset" else model_payloads()
files = sorted(payloads)

if args.resolve_only:
    # Upstream's per-file metadata for the whole commit, as a helper reports what
    # upstream told it: a size and a git object id for every file, and an LFS
    # digest for the LFS-managed ones.
    print(
        json.dumps(
            {
                "type": "result",
                "commit": COMMIT,
                "files": files,
                "repository_files": repository_files(payloads),
            },
            separators=(",", ":"),
        ),
        flush=True,
    )
    sys.exit(0)

if not args.output:
    parser.error("--output is required unless --resolve-only is given")

output = Path(args.output)
output.mkdir(parents=True, exist_ok=True)
# The selection is deliberately ignored: this fixture transfers the whole
# revision whatever it is asked for, which is what makes an over-broad
# acquisition visible in the archive rather than hidden by the fixture.
for path in files:
    target = output / path
    target.parent.mkdir(parents=True, exist_ok=True)
    target.write_bytes(payloads[path])
print(
    json.dumps(
        {
            "type": "result",
            "commit": COMMIT,
            "files": files,
            "repository_files": repository_files(payloads),
        },
        separators=(",", ":"),
    ),
    flush=True,
)
