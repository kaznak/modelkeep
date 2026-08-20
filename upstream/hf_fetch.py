#!/usr/bin/env python3
"""Official huggingface_hub based upstream acquisition helper for ModelKeep."""

import argparse
import json
import sys
from pathlib import Path

from huggingface_hub import HfApi, snapshot_download
from huggingface_hub.utils import GatedRepoError, HfHubHTTPError, RepositoryNotFoundError


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


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--repo-id", required=True)
    parser.add_argument("--revision", required=True)
    parser.add_argument("--output", required=True)
    parser.add_argument("--file", action="append", dest="files")
    args = parser.parse_args()

    output = Path(args.output)
    output.mkdir(parents=True, exist_ok=True)
    api = HfApi()
    info = api.repo_info(args.repo_id, revision=args.revision, repo_type="model")
    snapshot_download(
        repo_id=args.repo_id,
        revision=args.revision,
        repo_type="model",
        local_dir=str(output),
        allow_patterns=args.files or None,
    )
    files = safe_relative_files(output)
    if args.files:
        requested = set(args.files)
        files = [path for path in files if path in requested]
    print(json.dumps({"commit": info.sha, "files": files}, separators=(",", ":")))


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
