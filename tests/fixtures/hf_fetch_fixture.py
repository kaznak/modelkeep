#!/usr/bin/env python3
"""Upstream fixture implementing the fetch helper's stdout event contract.

It answers both helper modes: a resolve-only invocation, which reports the
commit and its per-file upstream metadata without transferring anything, and an
acquisition, which materializes the payload. The resolve-only answer is what lets
ModelKeep report a revision it has never archived (Issue 0074).
"""

import argparse
import json
import sys
from pathlib import Path


COMMIT = "a" * 40


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
    # Upstream's per-file metadata for the whole commit. No `blob_id` and no
    # `lfs_sha256`: a helper reports only what upstream told it.
    print(
        json.dumps(
            {
                "type": "result",
                "commit": COMMIT,
                "files": files,
                "repository_files": [
                    {"path": path, "size": len(payloads[path])} for path in files
                ],
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
            "repository_files": [
                {"path": path, "size": len(payloads[path])} for path in files
            ],
        },
        separators=(",", ":"),
    ),
    flush=True,
)
