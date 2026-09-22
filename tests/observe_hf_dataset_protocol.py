#!/usr/bin/env python3
"""Safely observe the public Hugging Face dataset download protocol."""

import argparse
import hashlib
import json
import os
import tempfile
from datetime import datetime, timezone
from pathlib import Path
from urllib.parse import urlsplit

import httpx
from huggingface_hub import HfApi, __version__ as huggingface_hub_version
from huggingface_hub import set_client_factory, snapshot_download


DATASET_REPO = "lhoestq/demo1"
DATASET_COMMIT = "87ecf163bedca9d80598b528940a9c4f99e14c11"
DATASET_FILE = "data/test.csv"
EXPECTED_FILES = {".gitattributes", "README.md", "data/test.csv", "data/train.csv"}
SAFE_FAILURE = "dataset protocol observation failed; details redacted\n"


def safe_trace_entry(response):
    entry = {
        "method": response.request.method,
        "host": response.request.url.host,
        "path": response.request.url.path,
        "status": response.status_code,
    }
    for header in ("accept-ranges", "content-range", "x-repo-commit"):
        if header in response.headers:
            entry[header] = response.headers[header]
    location = response.headers.get("location")
    if location:
        redirect_host = urlsplit(location).hostname
        if redirect_host:
            entry["redirect_host"] = redirect_host
    return entry


def sha256(path):
    digest = hashlib.sha256()
    with Path(path).open("rb") as source:
        for block in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


def observe():
    traces = []

    def client_factory():
        return httpx.Client(
            follow_redirects=True,
            event_hooks={"response": [lambda response: traces.append(safe_trace_entry(response))]},
        )

    set_client_factory(client_factory)
    info = HfApi(token=False).repo_info(
        DATASET_REPO,
        repo_type="dataset",
        revision=DATASET_COMMIT,
        files_metadata=True,
        token=False,
    )
    reported_files = {item.rfilename for item in info.siblings or []}

    with tempfile.TemporaryDirectory(prefix="modelkeep-hf-dataset-observation-") as temporary:
        snapshot = Path(
            snapshot_download(
                DATASET_REPO,
                repo_type="dataset",
                revision=DATASET_COMMIT,
                allow_patterns=[DATASET_FILE],
                token=False,
                cache_dir=Path(temporary) / "cache",
            )
        )
        payload = snapshot / DATASET_FILE
        payload_size = payload.stat().st_size
        payload_sha256 = sha256(payload)

    metadata_path = f"/api/datasets/{DATASET_REPO}/revision/{DATASET_COMMIT}"
    tree_path = f"/api/datasets/{DATASET_REPO}/tree/{DATASET_COMMIT}"
    resolve_path = f"/datasets/{DATASET_REPO}/resolve/{DATASET_COMMIT}/{DATASET_FILE}"
    rows = {
        "metadata": any(item["method"] == "GET" and item["path"] == metadata_path for item in traces),
        "tree": any(item["method"] == "GET" and item["path"] == tree_path for item in traces),
        "head": any(item["method"] == "HEAD" and item["path"] == resolve_path for item in traces),
        "payload": payload_size > 0 and payload_sha256 != hashlib.sha256(b"").hexdigest(),
        "revision": info.sha == DATASET_COMMIT,
        "inventory": EXPECTED_FILES.issubset(reported_files),
    }
    if not all(rows.values()):
        raise RuntimeError("incomplete dataset observation matrix")
    return {
        "schema": 1,
        "observed_at": datetime.now(timezone.utc).date().isoformat(),
        "huggingface_hub": huggingface_hub_version,
        "repository": {"repo_type": "dataset", "repo_id": DATASET_REPO, "commit": DATASET_COMMIT},
        "file": {"path": DATASET_FILE, "size": payload_size, "sha256": payload_sha256},
        "matrix": rows,
        "trace": traces,
    }


def main(observe_fn=None):
    parser = argparse.ArgumentParser()
    parser.add_argument("--output", type=Path)
    args = parser.parse_args()
    if os.environ.get("HF_TOKEN") or os.environ.get("HUGGING_FACE_HUB_TOKEN"):
        raise SystemExit("refusing to observe while a Hugging Face token is exported")
    try:
        encoded = json.dumps((observe_fn or observe)(), indent=2, sort_keys=True) + "\n"
        if args.output:
            args.output.write_text(encoded)
        else:
            print(encoded, end="")
    except Exception:
        parser.exit(1, SAFE_FAILURE)


if __name__ == "__main__":
    main()
