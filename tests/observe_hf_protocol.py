#!/usr/bin/env python3
"""Run the optional, online Hugging Face protocol observation matrix.

This script intentionally records only a safe, stable summary. It never emits
request query strings, authorization headers, cookies, or signed redirect URLs.
The deterministic CI contract lives in hf_client_integration.py; this script is
for periodically refreshing evidence about the real public Hub.
"""

import argparse
import hashlib
import json
import os
import tempfile
from datetime import datetime, timezone
from pathlib import Path
from urllib.parse import urlsplit

import httpx
from huggingface_hub import (
    HfApi,
    __version__ as huggingface_hub_version,
    get_hf_file_metadata,
    hf_hub_download,
    hf_hub_url,
    set_client_factory,
    snapshot_download,
)


PUBLIC_REPO = "optimum-intel-internal-testing/tiny-random-bert"
PUBLIC_COMMIT = "8301186e0724d418a8d4f5eb17011c9226f5b766"
SAFETENSORS_FILE = "model.safetensors"
SAFETENSORS_SHA256 = "965f02b6a7e5520fc12f710e4e3b6132f697f1c8f648819553c5ade86752d2de"

SHARDED_REPO = "bumblebee-testing/tiny-random-GPT2Model-sharded"
SHARDED_COMMIT = "4fca22a84867aacca5dcf7317144782ea1807e1a"
SHARDED_FILES = [
    "model-00001-of-00003.safetensors",
    "model-00002-of-00003.safetensors",
    "model-00003-of-00003.safetensors",
    "model.safetensors.index.json",
]
SAFE_FAILURE = "protocol observation failed; details redacted\n"


def safe_trace_entry(response):
    request = response.request
    entry = {
        "method": request.method,
        "host": request.url.host,
        "path": request.url.path,
        "status": response.status_code,
    }
    if "range" in request.headers:
        entry["range"] = request.headers["range"]
    for header in ("accept-ranges", "content-range", "x-repo-commit"):
        if header in response.headers:
            entry[header] = response.headers[header]
    location = response.headers.get("location")
    if location:
        entry["redirect_host"] = urlsplit(location).hostname
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
    api = HfApi(token=False)
    public_info = api.model_info(
        PUBLIC_REPO,
        revision=PUBLIC_COMMIT,
        files_metadata=True,
        token=False,
    )
    sharded_info = api.model_info(
        SHARDED_REPO,
        revision=SHARDED_COMMIT,
        files_metadata=True,
        token=False,
    )

    metadata = get_hf_file_metadata(
        hf_hub_url(PUBLIC_REPO, SAFETENSORS_FILE, revision=PUBLIC_COMMIT),
        token=False,
    )
    resolve_host = "huggingface.co"
    payload_host = urlsplit(metadata.location).hostname

    with tempfile.TemporaryDirectory(prefix="modelkeep-hf-observation-") as temporary:
        root = Path(temporary)
        config = hf_hub_download(
            PUBLIC_REPO,
            "config.json",
            revision=PUBLIC_COMMIT,
            token=False,
            cache_dir=root / "cache",
        )
        safetensors = hf_hub_download(
            PUBLIC_REPO,
            SAFETENSORS_FILE,
            revision=PUBLIC_COMMIT,
            token=False,
            cache_dir=root / "cache",
        )
        sharded = Path(
            snapshot_download(
                SHARDED_REPO,
                revision=SHARDED_COMMIT,
                allow_patterns=["config.json", "model*.safetensors", "model.safetensors.index.json"],
                token=False,
                cache_dir=root / "cache",
            )
        )
        assert Path(config).stat().st_size > 0
        assert sha256(safetensors) == SAFETENSORS_SHA256
        assert all((sharded / filename).is_file() for filename in SHARDED_FILES)

    # One byte is enough to observe payload Range behavior. Do not retain it.
    with httpx.Client(follow_redirects=True) as client:
        range_response = client.get(
            hf_hub_url(PUBLIC_REPO, SAFETENSORS_FILE, revision=PUBLIC_COMMIT),
            headers={"Range": "bytes=0-0"},
        )
        range_response.raise_for_status()
        assert len(range_response.content) == 1
    range_trace = [
        safe_trace_entry(response)
        for response in [*range_response.history, range_response]
    ]

    public_files = {item.rfilename for item in public_info.siblings or []}
    sharded_files = {item.rfilename for item in sharded_info.siblings or []}
    rows = {
        "public": public_info.sha == PUBLIC_COMMIT and "config.json" in public_files,
        "safetensors": SAFETENSORS_FILE in public_files,
        "sharded": all(filename in sharded_files for filename in SHARDED_FILES),
        "revision": public_info.sha == PUBLIC_COMMIT and sharded_info.sha == SHARDED_COMMIT,
        "head": any(
            item["method"] == "HEAD" and item["path"].endswith("/model.safetensors")
            for item in traces
        ),
        "range": range_response.status_code == 206
        and range_response.headers.get("content-range", "").startswith("bytes 0-0/"),
        "redirect": payload_host is not None and payload_host != resolve_host,
        "xet": metadata.xet_file_data is not None,
    }
    if not all(rows.values()):
        raise RuntimeError(f"incomplete observation matrix: {rows}")
    return {
        "schema": 1,
        "observed_at": datetime.now(timezone.utc).date().isoformat(),
        "huggingface_hub": huggingface_hub_version,
        "repositories": {
            PUBLIC_REPO: PUBLIC_COMMIT,
            SHARDED_REPO: SHARDED_COMMIT,
        },
        "matrix": rows,
        "metadata": {
            "commit": metadata.commit_hash,
            "etag": metadata.etag,
            "size": metadata.size,
            "redirect_host": payload_host,
            "xet": metadata.xet_file_data is not None,
        },
        "range_trace": range_trace,
        "trace": traces,
    }


def main(observe_fn=None):
    parser = argparse.ArgumentParser()
    parser.add_argument("--output", type=Path)
    args = parser.parse_args()
    if os.environ.get("HF_TOKEN") or os.environ.get("HUGGING_FACE_HUB_TOKEN"):
        raise SystemExit("refusing to observe while a Hugging Face token is exported")
    try:
        result = (observe_fn or observe)()
        encoded = json.dumps(result, indent=2, sort_keys=True) + "\n"
        if args.output:
            args.output.write_text(encoded)
        else:
            print(encoded, end="")
    except Exception:
        parser.exit(1, SAFE_FAILURE)


if __name__ == "__main__":
    main()
