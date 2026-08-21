#!/usr/bin/env python3
import argparse
import json
import sys
from pathlib import Path


COMMIT = "a" * 40


parser = argparse.ArgumentParser()
parser.add_argument("--repo-id", required=True)
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
(output / "config.json").write_text('{"model_type":"modelkeep-fixture"}')
(output / "tokenizer.json").write_text('{"version":"1.0"}')
print(json.dumps({"commit": COMMIT, "files": ["config.json", "tokenizer.json"]}))
