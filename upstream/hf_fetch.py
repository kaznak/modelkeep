#!/usr/bin/env python3
"""Official huggingface_hub based upstream acquisition helper for ModelKeep."""

import argparse
import contextlib
import fnmatch
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
        self.last_emit = {}
        self.last_event = {}
        self.output = None
        self.expected = {}
        self.completed_files = -1
        self.completed_bytes = -1
        self.download_finalized = False

    def emit(self, event, force=False):
        now = time.monotonic()
        key = (event["phase"], event.get("unit"))
        with self.lock:
            if not force and self.last_event.get(key) == event:
                return
            if not force and now - self.last_emit.get(key, 0.0) < self.minimum_interval:
                return
            self.last_emit[key] = now
            self.last_event[key] = event.copy()
            print(json.dumps({"type": "progress", "version": 1, **event}, separators=(",", ":")), file=self.stream, flush=True)

    def phase(self, phase):
        self.emit({"phase": phase}, force=True)

    def set_expected(self, output, expected):
        self.output = Path(output)
        self.expected = {path: size for path, size in expected}
        if not self.expected:
            self.phase("downloading")
            return
        self._report_files(force=True)

    def _report_files(self, force=False, finalized=False):
        if self.output is None or self.download_finalized:
            return
        completed = 0
        completed_bytes = 0
        for relative, size in self.expected.items():
            path = self.output / relative
            try:
                metadata = path.stat()
            except OSError:
                continue
            if path.is_file() and ((size is not None and metadata.st_size == size) or (size is None and finalized)):
                completed += 1
                completed_bytes += metadata.st_size
        download_metadata = self.output / ".cache" / "huggingface" / "download"
        try:
            incomplete = download_metadata.glob("*.incomplete")
            partial_bytes = sum(
                path.stat().st_size for path in incomplete if path.is_file()
            )
        except OSError:
            partial_bytes = 0
        completed_bytes += partial_bytes
        if self.expected and all(size is not None for size in self.expected.values()):
            completed_bytes = min(completed_bytes, sum(self.expected.values()))
        changed = completed != self.completed_files
        bytes_changed = completed_bytes != self.completed_bytes
        self.completed_files = completed
        self.completed_bytes = completed_bytes
        byte_event = {
            "phase": "downloading",
            "unit": "bytes",
            "completed": completed_bytes,
        }
        if self.expected and all(size is not None for size in self.expected.values()):
            byte_event["total"] = sum(self.expected.values())
        self.emit(byte_event, force=force or bytes_changed)
        self.emit(
            {
                "phase": "downloading",
                "unit": "files",
                "completed": completed,
                "total": len(self.expected),
            },
            force=force or changed,
        )
        if finalized:
            self.download_finalized = True

    def tqdm_class(self):
        reporter = self

        class ReportingTqdm(tqdm):
            def __init__(self, *args, **kwargs):
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
                reporter._report_files()

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
        if relative.parts[0].startswith(".modelkeep-"):
            continue
        result.append(relative.as_posix())
    return sorted(result)


def normalized_pattern(pattern):
    """Mirrors the official client's pattern normalization.

    `huggingface_hub.utils.filter_repo_objects` rewrites separators and expands
    a trailing `/` into a directory wildcard before matching with
    `fnmatch.fnmatchcase`. The expected-file set and the post-download filter
    must agree with what the client actually transfers.
    """
    pattern = str(pattern).replace("\\", "/")
    return pattern + "*" if pattern.endswith("/") else pattern


def selected(path, include=None, exclude=None):
    path = str(path).replace("\\", "/")
    if include and not any(
        fnmatch.fnmatchcase(path, normalized_pattern(pattern)) for pattern in include
    ):
        return False
    if exclude and any(
        fnmatch.fnmatchcase(path, normalized_pattern(pattern)) for pattern in exclude
    ):
        return False
    return True


def expected_files(info, patterns=None, exclude=None):
    result = []
    for sibling in getattr(info, "siblings", None) or []:
        path = getattr(sibling, "rfilename", None)
        if not isinstance(path, str) or not path or ".cache" in Path(path).parts:
            continue
        if Path(path).parts[0].startswith(".modelkeep-"):
            continue
        if not selected(path, patterns, exclude):
            continue
        size = getattr(sibling, "size", None)
        result.append((path, size if isinstance(size, int) and size >= 0 else None))
    return sorted(result)


def resolve(
    repo_id,
    requested_revision,
    files=None,
    exclude=None,
    repo_type="model",
    api=None,
    progress=None,
):
    """Resolves the commit and the upstream files the selection covers.

    Transfers nothing. The `resolved` event is emitted here so that both the
    acquisition and the resolve-only mode announce the immutable commit the same
    way.
    """
    api = api or HfApi()
    if progress is not None:
        progress.phase("resolving_revision")
    # stdout is the machine-readable ModelKeep event channel. Official client and
    # transport implementations may write diagnostics to stdout, especially while
    # recovering an interrupted transfer. Keep that output away from the protocol;
    # the Rust parent deliberately discards helper stderr to avoid credential leaks.
    with contextlib.redirect_stdout(sys.stderr):
        info = api.repo_info(
            repo_id,
            revision=requested_revision,
            repo_type=repo_type,
            files_metadata=True,
        )
    commit = info.sha
    if not isinstance(commit, str) or not COMMIT_PATTERN.fullmatch(commit):
        raise ValueError("upstream returned malformed commit identity")
    print(
        json.dumps({"type": "resolved", "version": 1, "commit": commit}, separators=(",", ":")),
        flush=True,
    )
    return commit, expected_files(info, files, exclude)


def inventory(
    repo_id,
    requested_revision,
    files=None,
    exclude=None,
    repo_type="model",
    api=None,
    progress=None,
):
    """Resolve-only answer: which upstream paths the selection covers.

    The payload keeps the `result` event's existing shape — `commit` plus a list
    of path strings — and adds `sizes` for the paths whose size upstream
    reported. An empty `files` list is a legitimate answer here: it means
    upstream holds nothing matching the selection.
    """
    commit, expected = resolve(
        repo_id,
        requested_revision,
        files=files,
        exclude=exclude,
        repo_type=repo_type,
        api=api,
        progress=progress,
    )
    return {
        "commit": commit,
        "files": [path for path, _ in expected],
        "sizes": {path: size for path, size in expected},
    }


def acquire(
    repo_id,
    requested_revision,
    output,
    files=None,
    exclude=None,
    repo_type="model",
    api=None,
    download=None,
    progress=None,
):
    output = Path(output)
    output.mkdir(parents=True, exist_ok=True)
    api = api or HfApi()
    download = download or snapshot_download

    commit, expected = resolve(
        repo_id,
        requested_revision,
        files=files,
        exclude=exclude,
        repo_type=repo_type,
        api=api,
        progress=progress,
    )
    if progress is not None:
        progress.set_expected(output, expected)

    download_kwargs = dict(
        repo_id=repo_id,
        revision=commit,
        repo_type=repo_type,
        local_dir=str(output),
        allow_patterns=files or None,
        ignore_patterns=exclude or None,
    )
    if progress is not None:
        download_kwargs["tqdm_class"] = progress.tqdm_class()
    with contextlib.redirect_stdout(sys.stderr):
        download(**download_kwargs)
    archived_files = safe_relative_files(output)
    if files or exclude:
        archived_files = [
            path for path in archived_files if selected(path, files, exclude)
        ]
    if progress is not None:
        if not expected:
            progress.set_expected(output, [(path, None) for path in archived_files])
        progress._report_files(force=True, finalized=True)
        progress.phase("inventorying_snapshot")
    return {"commit": commit, "files": archived_files}


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--repo-id", required=True)
    parser.add_argument("--repo-type", choices=("model", "dataset"), default="model")
    parser.add_argument("--revision", required=True)
    parser.add_argument("--output")
    parser.add_argument("--file", action="append", dest="files")
    parser.add_argument("--exclude", action="append", dest="exclude")
    parser.add_argument("--resolve-only", action="store_true", dest="resolve_only")
    args = parser.parse_args()

    if args.resolve_only:
        result = inventory(
            repo_id=args.repo_id,
            repo_type=args.repo_type,
            requested_revision=args.revision,
            files=args.files,
            exclude=args.exclude,
        )
    else:
        if not args.output:
            parser.error("--output is required unless --resolve-only is given")
        result = acquire(
            repo_id=args.repo_id,
            repo_type=args.repo_type,
            requested_revision=args.revision,
            output=args.output,
            files=args.files,
            exclude=args.exclude,
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
