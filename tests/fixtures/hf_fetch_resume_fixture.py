#!/usr/bin/env python3
"""Resume through the production helper boundary using retained partial state."""

import argparse
import json
from pathlib import Path
from types import SimpleNamespace

import hf_fetch


COMMIT = "c" * 40
PARTIAL = b"incomplete-model-payload"
PAYLOAD = PARTIAL + b"-resumed"


class ResumeApi:
    def repo_info(self, repo_id, revision, repo_type, files_metadata=False):
        if (
            repo_id != "org/crash"
            or revision != COMMIT
            or repo_type != "model"
            or not files_metadata
        ):
            raise RuntimeError("unexpected resumed metadata request")
        return SimpleNamespace(
            sha=COMMIT,
            siblings=[SimpleNamespace(rfilename="partial.bin", size=len(PAYLOAD))],
        )


def resume_download(**kwargs):
    output = Path(kwargs["local_dir"])
    partial = output / "partial.bin"
    if kwargs["revision"] != COMMIT or partial.read_bytes() != PARTIAL:
        raise RuntimeError("retained partial staging was not reused")

    # Model an official client diagnostic observed only while resuming. It must not
    # enter the ModelKeep JSON event channel.
    print('{"resumed":true,"transport":"diagnostic"}')
    partial.write_bytes(PAYLOAD)


parser = argparse.ArgumentParser()
parser.add_argument("--repo-id", required=True)
parser.add_argument("--repo-type", choices=("model", "dataset"), default="model")
parser.add_argument("--revision", required=True)
parser.add_argument("--output", required=True)
parser.add_argument("--file", action="append", dest="files")
args = parser.parse_args()

result = hf_fetch.acquire(
    repo_id=args.repo_id,
    repo_type=args.repo_type,
    requested_revision=args.revision,
    output=args.output,
    files=args.files,
    api=ResumeApi(),
    download=resume_download,
    progress=hf_fetch.ProgressReporter(),
)
print(json.dumps({"type": "result", **result}, separators=(",", ":")), flush=True)
