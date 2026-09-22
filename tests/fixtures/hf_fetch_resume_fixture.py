#!/usr/bin/env python3
"""Completes the crash fixture only when its partial payload is reused."""

import argparse
import json
from pathlib import Path


parser = argparse.ArgumentParser()
parser.add_argument("--repo-id", required=True)
parser.add_argument("--revision", required=True)
parser.add_argument("--output", required=True)
parser.add_argument("--file", action="append")
args = parser.parse_args()

output = Path(args.output)
partial = output / "partial.bin"
expected = b"incomplete-model-payload"
if args.revision != "c" * 40 or not partial.is_file() or partial.read_bytes() != expected:
    raise SystemExit(1)

payload = expected + b"-resumed"
partial.write_bytes(payload)
print(json.dumps({"type": "resolved", "version": 1, "commit": "c" * 40}), flush=True)
print(
    json.dumps(
        {"type": "result", "commit": "c" * 40, "files": ["partial.bin"]}
    ),
    flush=True,
)
