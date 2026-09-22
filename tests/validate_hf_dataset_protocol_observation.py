#!/usr/bin/env python3
"""Offline validation for the sanitized dataset protocol observation."""

import json
import re
import sys
from pathlib import Path

import observe_hf_dataset_protocol as observer


EXPECTED_MATRIX = {"metadata", "tree", "head", "payload", "revision", "inventory"}


def validate(path):
    record = json.loads(Path(path).read_text())
    assert record["schema"] == 1
    assert record["huggingface_hub"] == "1.27.0"
    assert re.fullmatch(r"\d{4}-\d{2}-\d{2}", record["observed_at"])
    assert record["repository"] == {
        "repo_type": "dataset",
        "repo_id": observer.DATASET_REPO,
        "commit": observer.DATASET_COMMIT,
    }
    assert record["file"]["path"] == observer.DATASET_FILE
    assert record["file"]["size"] > 0
    assert re.fullmatch(r"[0-9a-f]{64}", record["file"]["sha256"])
    assert set(record["matrix"]) == EXPECTED_MATRIX
    assert all(record["matrix"].values())

    required = {
        ("GET", f"/api/datasets/{observer.DATASET_REPO}/revision/{observer.DATASET_COMMIT}"),
        ("GET", f"/api/datasets/{observer.DATASET_REPO}/tree/{observer.DATASET_COMMIT}"),
        ("HEAD", f"/datasets/{observer.DATASET_REPO}/resolve/{observer.DATASET_COMMIT}/{observer.DATASET_FILE}"),
    }
    observed = {(item["method"], item["path"]) for item in record["trace"]}
    assert required.issubset(observed)
    safe_trace_keys = {
        "method",
        "host",
        "path",
        "status",
        "accept-ranges",
        "content-range",
        "x-repo-commit",
        "redirect_host",
    }
    assert all(set(item).issubset(safe_trace_keys) for item in record["trace"])

    encoded = json.dumps(record)
    for forbidden in ["?", "authorization", "cookie", "HF_TOKEN", "Bearer ", "Traceback"]:
        assert forbidden not in encoded


if __name__ == "__main__":
    validate(sys.argv[1])
