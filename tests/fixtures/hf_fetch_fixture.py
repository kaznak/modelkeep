#!/usr/bin/env python3
import argparse
import json
from pathlib import Path


COMMIT = "a" * 40


parser = argparse.ArgumentParser()
parser.add_argument("--repo-id", required=True)
parser.add_argument("--revision", required=True)
parser.add_argument("--output", required=True)
parser.add_argument("--file", action="append")
args = parser.parse_args()

output = Path(args.output)
output.mkdir(parents=True, exist_ok=True)
(output / "config.json").write_text('{"model_type":"modelkeep-fixture"}')
(output / "tokenizer.json").write_text('{"version":"1.0"}')
print(json.dumps({"commit": COMMIT, "files": ["config.json", "tokenizer.json"]}))
