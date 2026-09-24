#!/usr/bin/env python3
"""Upstream fixture whose revision contains same-size and byte-identical files.

Issue 0078: a validator that mixes the commit with the file length collides for
any two files of equal size in one revision, and sharded weights are uniform in
size by construction. Every other fixture in this repository happens to hold
files whose lengths differ, so none of them can exhibit the collision.

This fixture pins both halves of the content-addressed contract:

- four equally sized shards whose bytes differ, so a size-derived validator
  collides for all four while a content-derived one does not;
- two byte-identical files, so a client that stores one blob for both is
  observed to be correct rather than corrupt.
"""

import argparse
import json
import sys
from pathlib import Path


COMMIT = "d" * 40
SHARD_COUNT = 4
SHARD_SIZE = 4096
DUPLICATE_BYTES = b'{"content":"modelkeep-identical-payload"}'


def shard_name(index):
    return f"model-{index:05d}-of-{SHARD_COUNT:05d}.safetensors"


def shard_bytes(index):
    """Equal length for every shard, distinct bytes for each.

    The marker sits in the middle rather than at the front so a comparison that
    only samples a prefix cannot tell the shards apart either.
    """
    marker = f"MODELKEEP-COLLISION-SHARD-{index:05d}".encode()
    head = b"\x00" * ((SHARD_SIZE - len(marker)) // 2)
    tail = b"\x00" * (SHARD_SIZE - len(marker) - len(head))
    return head + marker + tail


def payloads():
    files = {
        "config.json": b'{"model_type":"modelkeep-collision-fixture"}',
        "duplicate-one.json": DUPLICATE_BYTES,
        "duplicate-two.json": DUPLICATE_BYTES,
    }
    for index in range(1, SHARD_COUNT + 1):
        files[shard_name(index)] = shard_bytes(index)
    files["model.safetensors.index.json"] = json.dumps(
        {
            "metadata": {"total_size": SHARD_COUNT * SHARD_SIZE},
            "weight_map": {
                f"layer.{index - 1}": shard_name(index)
                for index in range(1, SHARD_COUNT + 1)
            },
        },
        separators=(",", ":"),
    ).encode()
    return files


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--repo-id", required=True)
    parser.add_argument("--repo-type", choices=("model", "dataset"), default="model")
    parser.add_argument("--revision", required=True)
    parser.add_argument("--output")
    parser.add_argument("--file", action="append")
    parser.add_argument("--exclude", action="append")
    parser.add_argument("--resolve-only", action="store_true", dest="resolve_only")
    args = parser.parse_args()

    if args.resolve_only:
        # The commit's whole file list with upstream's per-file metadata, which is
        # what ModelKeep answers repository metadata from before it has archived
        # anything (Issue 0074).
        files = payloads()
        print(
            json.dumps(
                {
                    "type": "result",
                    "commit": COMMIT,
                    "files": sorted(files),
                    "repository_files": [
                        {"path": path, "size": len(files[path])}
                        for path in sorted(files)
                    ],
                },
                separators=(",", ":"),
            ),
            flush=True,
        )
        return
    if not args.output:
        parser.error("--output is required unless --resolve-only is given")

    output = Path(args.output)
    output.mkdir(parents=True, exist_ok=True)
    selected = payloads()
    if args.file:
        selected = {path: selected[path] for path in args.file if path in selected}
    for path, content in selected.items():
        target = output / path
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_bytes(content)
    print(
        json.dumps(
            {"type": "result", "commit": COMMIT, "files": sorted(selected)},
            separators=(",", ":"),
        ),
        flush=True,
    )


if __name__ == "__main__":
    sys.exit(main())
