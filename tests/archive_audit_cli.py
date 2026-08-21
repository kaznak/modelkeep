#!/usr/bin/env python3
import json
import subprocess
import sys
import tempfile
from pathlib import Path

COMMIT = "a" * 40


def audit(binary, archive):
    return subprocess.run(
        [binary, "audit", archive],
        check=False,
        capture_output=True,
        text=True,
    )


def main():
    binary = Path(sys.argv[1])
    with tempfile.TemporaryDirectory() as temporary:
        root = Path(temporary)
        cache = root / "cache" / "models--org--model"
        blob = cache / "blobs/config"
        snapshot = cache / "snapshots" / COMMIT
        blob.parent.mkdir(parents=True)
        snapshot.mkdir(parents=True)
        blob.write_bytes(b"healthy")
        (snapshot / "config.json").symlink_to(blob)

        archive = root / "archive"
        subprocess.run(
            [binary, "import-hf-cache", root / "cache", archive], check=True
        )

        clean = audit(binary, archive)
        assert clean.returncode == 0, clean.stderr
        clean_report = json.loads(clean.stdout)
        assert clean_report == {"checked": 1, "failures": [], "status": "clean"}

        archived_file = (
            archive
            / "models"
            / "org"
            / "model"
            / "revisions"
            / COMMIT
            / "config.json"
        )
        archived_file.write_bytes(b"corrupt")

        failed = audit(binary, archive)
        assert failed.returncode != 0
        failed_report = json.loads(failed.stdout)
        assert failed_report["status"] == "failed"
        assert failed_report["checked"] == 1
        assert len(failed_report["failures"]) == 1
        assert failed_report["failures"][0]["repo_id"] == "org/model"
        assert failed_report["failures"][0]["commit"] == COMMIT


if __name__ == "__main__":
    main()
