#!/usr/bin/env python3
"""Offline validation for a committed, sanitized HF observation record."""

import json
import re
import sys
from pathlib import Path

import observe_hf_protocol as observer


EXPECTED_MATRIX = {
    "public",
    "safetensors",
    "sharded",
    "revision",
    "head",
    "range",
    "redirect",
    "xet",
}


def validate(path):
    record = json.loads(Path(path).read_text())
    assert record["schema"] == 1
    assert record["huggingface_hub"] == "1.27.0"
    assert re.fullmatch(r"\d{4}-\d{2}-\d{2}", record["observed_at"])
    assert set(record["matrix"]) == EXPECTED_MATRIX
    assert all(record["matrix"].values())
    assert record["repositories"] == {
        observer.PUBLIC_REPO: observer.PUBLIC_COMMIT,
        observer.SHARDED_REPO: observer.SHARDED_COMMIT,
    }
    assert record["metadata"]["commit"] == observer.PUBLIC_COMMIT
    assert record["metadata"]["etag"] == observer.SAFETENSORS_SHA256
    assert record["metadata"]["xet"] is True
    assert record["metadata"]["redirect_host"]
    assert any(
        item["method"] == "HEAD"
        and item["path"].endswith(f"/{observer.SAFETENSORS_FILE}")
        for item in record["trace"]
    )
    assert any(
        item["status"] == 206 and item.get("range") == "bytes=0-0"
        for item in record["range_trace"]
    )

    encoded = json.dumps(record)
    for forbidden in ["?", "authorization", "cookie", "HF_TOKEN", "Bearer "]:
        assert forbidden not in encoded


if __name__ == "__main__":
    validate(sys.argv[1])
