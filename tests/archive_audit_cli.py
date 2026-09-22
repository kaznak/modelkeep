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
        for repo_type, payload in (("models", b"model"), ("datasets", b"dataset")):
            cache = root / "cache" / f"{repo_type}--org--shared"
            blob = cache / "blobs/config"
            snapshot = cache / "snapshots" / COMMIT
            blob.parent.mkdir(parents=True)
            snapshot.mkdir(parents=True)
            blob.write_bytes(payload)
            (snapshot / "config.json").symlink_to(blob)

        archive = root / "archive"
        subprocess.run(
            [binary, "import-hf-cache", root / "cache", archive], check=True
        )

        clean = audit(binary, archive)
        assert clean.returncode == 0, clean.stderr
        clean_report = json.loads(clean.stdout)
        assert clean_report == {"checked": 2, "failures": [], "status": "clean"}

        model_list = subprocess.run(
            [binary, "list", archive, "org/shared"],
            check=True,
            capture_output=True,
            text=True,
        )
        dataset_list = subprocess.run(
            [
                binary,
                "list",
                archive,
                "org/shared",
                "--repo-type",
                "dataset",
            ],
            check=True,
            capture_output=True,
            text=True,
        )
        assert model_list.stdout.strip() == COMMIT
        assert dataset_list.stdout.strip() == COMMIT

        subprocess.run(
            [
                binary,
                "verify",
                archive,
                "org/shared",
                COMMIT,
                "--repo-type",
                "dataset",
            ],
            check=True,
        )

        archived_file = (
            archive
            / "datasets"
            / "org"
            / "shared"
            / "revisions"
            / COMMIT
            / "config.json"
        )
        archived_file.write_bytes(b"corrupt")

        failed = audit(binary, archive)
        assert failed.returncode != 0
        failed_report = json.loads(failed.stdout)
        assert failed_report["status"] == "failed"
        assert failed_report["checked"] == 2
        assert len(failed_report["failures"]) == 1
        assert failed_report["failures"][0]["repo_type"] == "dataset"
        assert failed_report["failures"][0]["repo_id"] == "org/shared"
        assert failed_report["failures"][0]["commit"] == COMMIT


if __name__ == "__main__":
    main()
