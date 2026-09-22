#!/usr/bin/env python3
"""Upstream fixture that remains blocked after writing an observable partial file."""

import argparse
import json
import time
from pathlib import Path


parser = argparse.ArgumentParser()
parser.add_argument("--repo-id", required=True)
parser.add_argument("--repo-type", choices=("model", "dataset"), default="model")
parser.add_argument("--revision", required=True)
parser.add_argument("--output", required=True)
parser.add_argument("--file", action="append")
args = parser.parse_args()

output = Path(args.output)
output.mkdir(parents=True, exist_ok=True)
print(json.dumps({"type": "resolved", "version": 1, "commit": "c" * 40}), flush=True)
(output / "partial.bin").write_bytes(b"incomplete-model-payload")
print(
    json.dumps(
        {
            "type": "progress",
            "phase": "downloading",
            "unit": "bytes",
            "completed": len(b"incomplete-model-payload"),
            "total": 1024 * 1024,
        }
    ),
    flush=True,
)
time.sleep(300)
