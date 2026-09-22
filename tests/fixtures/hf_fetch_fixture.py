#!/usr/bin/env python3
import argparse
import json
import sys
from pathlib import Path


COMMIT = "a" * 40


parser = argparse.ArgumentParser()
parser.add_argument("--repo-id", required=True)
parser.add_argument("--repo-type", choices=("model", "dataset"), default="model")
parser.add_argument("--revision", required=True)
parser.add_argument("--output", required=True)
parser.add_argument("--file", action="append")
args = parser.parse_args()

if args.revision == "missing":
    sys.exit(11)
if args.revision == "private":
    sys.exit(12)
if args.revision == "unavailable":
    sys.exit(10)

output = Path(args.output)
output.mkdir(parents=True, exist_ok=True)
(output / "README.md").write_text(f"# ModelKeep {args.repo_type} fixture\n")
if args.repo_type == "dataset":
    data = output / "data"
    data.mkdir()
    (data / "test.csv").write_bytes(b"split,value\ntest,dataset\n")
    (data / "train.csv").write_bytes(b"split,value\ntrain,dataset\n")
    files = ["README.md", "data/test.csv", "data/train.csv"]
    print(json.dumps({"commit": COMMIT, "files": files}))
    sys.exit(0)

(output / "config.json").write_text('{"model_type":"modelkeep-fixture"}')
(output / "tokenizer.json").write_text('{"version":"1.0"}')
(output / "model.safetensors").write_bytes(b"MODELKEEP-SAFETENSORS-FIXTURE")
(output / "model-00001-of-00002.safetensors").write_bytes(b"MODELKEEP-SHARD-ONE")
(output / "model-00002-of-00002.safetensors").write_bytes(b"MODELKEEP-SHARD-TWO")
(output / "model.safetensors.index.json").write_text(
    json.dumps(
        {
            "metadata": {"total_size": 38},
            "weight_map": {
                "layer.0": "model-00001-of-00002.safetensors",
                "layer.1": "model-00002-of-00002.safetensors",
            },
        },
        separators=(",", ":"),
    )
)
files = [
    "README.md",
    "config.json",
    "model-00001-of-00002.safetensors",
    "model-00002-of-00002.safetensors",
    "model.safetensors",
    "model.safetensors.index.json",
    "tokenizer.json",
]
print(json.dumps({"commit": COMMIT, "files": files}))
