#!/usr/bin/env python3
"""Official huggingface_hub based upstream acquisition helper for ModelKeep."""

import argparse
import json
import re
import sys
from pathlib import Path

from huggingface_hub import HfApi, snapshot_download
from huggingface_hub.utils import GatedRepoError, HfHubHTTPError, RepositoryNotFoundError


COMMIT_PATTERN = re.compile(r"^[0-9a-fA-F]{40}$")


def safe_relative_files(root: Path):
    result = []
    for path in root.rglob("*"):
        if not path.is_file():
            continue
        relative = path.relative_to(root)
        if any(part in ("", ".", "..") for part in relative.parts):
            raise ValueError("unsafe upstream path")
        if ".cache" in relative.parts:
            continue
        result.append(relative.as_posix())
    return sorted(result)


def acquire(repo_id, requested_revision, output, files=None, api=None, download=None):
    output = Path(output)
    output.mkdir(parents=True, exist_ok=True)
    api = api or HfApi()
    download = download or snapshot_download

    info = api.repo_info(repo_id, revision=requested_revision, repo_type="model")
    commit = info.sha
    if not isinstance(commit, str) or not COMMIT_PATTERN.fullmatch(commit):
        raise ValueError("upstream returned malformed commit identity")

    download(
        repo_id=repo_id,
        revision=commit,
        repo_type="model",
        local_dir=str(output),
        allow_patterns=files or None,
    )
    archived_files = safe_relative_files(output)
    if files:
        requested = set(files)
        archived_files = [path for path in archived_files if path in requested]
    return {"commit": commit, "files": archived_files}


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--repo-id", required=True)
    parser.add_argument("--revision", required=True)
    parser.add_argument("--output", required=True)
    parser.add_argument("--file", action="append", dest="files")
    args = parser.parse_args()

    result = acquire(
        repo_id=args.repo_id,
        requested_revision=args.revision,
        output=args.output,
        files=args.files,
    )
    print(json.dumps(result, separators=(",", ":")))


if __name__ == "__main__":
    try:
        main()
    except (RepositoryNotFoundError,) :
        sys.exit(11)
    except GatedRepoError:
        sys.exit(12)
    except HfHubHTTPError as error:
        if error.response is not None and error.response.status_code == 404:
            sys.exit(11)
        if error.response is not None and error.response.status_code in (401, 403):
            sys.exit(12)
        sys.exit(1)
    except (ConnectionError, TimeoutError):
        sys.exit(10)
    except Exception:
        sys.exit(1)
