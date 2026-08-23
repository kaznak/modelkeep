#!/usr/bin/env python3
"""Official huggingface_hub based upstream acquisition helper for ModelKeep."""

import argparse
import json
import re
import sys
import threading
import time
from pathlib import Path

from huggingface_hub import HfApi, snapshot_download
from huggingface_hub.utils import GatedRepoError, HfHubHTTPError, RepositoryNotFoundError
from tqdm.auto import tqdm


COMMIT_PATTERN = re.compile(r"^[0-9a-fA-F]{40}$")


class ProgressReporter:
    def __init__(self, stream=None, minimum_interval=5.0):
        self.stream = stream or sys.stdout
        self.minimum_interval = minimum_interval
        self.lock = threading.Lock()
        self.last_emit = 0.0

    def emit(self, event, force=False):
        now = time.monotonic()
        with self.lock:
            if not force and now - self.last_emit < self.minimum_interval:
                return
            self.last_emit = now
            print(json.dumps({"type": "progress", **event}, separators=(",", ":")), file=self.stream, flush=True)

    def tqdm_class(self):
        reporter = self

        class ReportingTqdm(tqdm):
            def __init__(self, *args, **kwargs):
                self._modelkeep_unit = kwargs.get("unit", "")
                super().__init__(*args, **kwargs)
                self._report()

            def display(self, *args, **kwargs):
                return None

            def update(self, n=1):
                result = super().update(n)
                self._report()
                return result

            def close(self):
                self._report(force=True)
                super().close()

            def _report(self, force=False):
                event = {
                    "phase": "downloading",
                    "unit": "bytes" if self._modelkeep_unit == "B" else "files",
                    "completed": int(self.n),
                }
                if self.total is not None:
                    event["total"] = int(self.total)
                reporter.emit(event, force=force)

        return ReportingTqdm


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


def acquire(repo_id, requested_revision, output, files=None, api=None, download=None, progress=None):
    output = Path(output)
    output.mkdir(parents=True, exist_ok=True)
    api = api or HfApi()
    download = download or snapshot_download

    info = api.repo_info(repo_id, revision=requested_revision, repo_type="model")
    commit = info.sha
    if not isinstance(commit, str) or not COMMIT_PATTERN.fullmatch(commit):
        raise ValueError("upstream returned malformed commit identity")

    download_kwargs = dict(
        repo_id=repo_id,
        revision=commit,
        repo_type="model",
        local_dir=str(output),
        allow_patterns=files or None,
    )
    if progress is not None:
        download_kwargs["tqdm_class"] = progress.tqdm_class()
    download(**download_kwargs)
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
        progress=ProgressReporter(),
    )
    print(json.dumps({"type": "result", **result}, separators=(",", ":")), flush=True)


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
