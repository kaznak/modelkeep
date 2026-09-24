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
import urllib.parse
from pathlib import Path

from huggingface_hub import HfApi, snapshot_download
from huggingface_hub.utils import (
    EntryNotFoundError,
    GatedRepoError,
    HfHubHTTPError,
    RepositoryNotFoundError,
    RevisionNotFoundError,
)
from tqdm.auto import tqdm


COMMIT_PATTERN = re.compile(r"^[0-9a-fA-F]{40}$")


# --- Failure reporting (Issue 0084) -----------------------------------------
#
# A failed acquisition has to say why. The helper is the only place that holds
# both the client's exception and the context needed to classify it, so the
# class is decided here and reported as a typed event on the same stdout
# protocol channel the Rust parent already drains. The parent never recovers a
# reason by pattern matching helper text, and helper stderr stays discarded:
# this event is the whole diagnostic surface, which is why the message is
# sanitized before it leaves this process.

FAILURE_EVENT_VERSION = 1

#: The longest sanitized message reported for one exception. A client message
#: is untrusted, unbounded text; a bound keeps one failure from flooding the
#: parent's log or management state.
MESSAGE_LIMIT = 400
EXCEPTION_NAME_LIMIT = 80
#: How many links of the exception chain are reported. The immediate cause is
#: routinely where the real reason is (a transport wrapping an OS error), and a
#: bound keeps a deep chain from crowding out the failure itself.
CAUSE_DEPTH = 2

#: Upstream could not be reached or could not answer.
CLASS_UNAVAILABLE = "unavailable"
#: Upstream answered that the repository, revision or file does not exist.
CLASS_NOT_FOUND = "not_found"
#: Upstream refused the credentials, or the repository is gated for them.
CLASS_UNAUTHORIZED = "unauthorized"
#: Upstream refused because the caller asked too often.
CLASS_RATE_LIMITED = "rate_limited"
#: The transfer or the client itself failed. This is also the honest answer for
#: an exception the helper cannot classify: the exception type and its sanitized
#: message are reported rather than a guessed class, because reporting
#: "unavailable" for an authorization problem would be worse than reporting
#: that only the client's own words are known.
CLASS_CLIENT_FAILURE = "client_failure"

#: The exit code for each class, so a parent that reads only the status still
#: learns the class. Codes 10, 11 and 12 are the ones the helper contract
#: already defined.
EXIT_CODES = {
    CLASS_UNAVAILABLE: 10,
    CLASS_NOT_FOUND: 11,
    CLASS_UNAUTHORIZED: 12,
    CLASS_RATE_LIMITED: 13,
    CLASS_CLIENT_FAILURE: 14,
}

REDACTED = "<redacted>"
REDACTED_URL = "<redacted-url>"
REDACTED_TOKEN = "<redacted-token>"
TRUNCATION_MARKER = "...[truncated]"

URL_PATTERN = re.compile(r"\b[a-zA-Z][a-zA-Z0-9+.\-]*://[^\s\"'<>`]*")


def redacted_url(match):
    """Reduces one URL to its endpoint identity.

    A signed URL carries its capability in the userinfo, path, query and
    fragment; the scheme, host and port are the endpoint identity an operator
    needs in order to tell a hub failure from a CAS failure, and they carry no
    signature. This is a structural reduction rather than a search for secrets:
    `urlsplit().hostname` drops userinfo, and everything after the authority is
    discarded whatever it contained.
    """
    try:
        parts = urllib.parse.urlsplit(match.group(0))
        host = parts.hostname or ""
        port = "" if parts.port is None else f":{parts.port}"
    except ValueError:
        return REDACTED_URL
    if not parts.scheme or not host:
        return REDACTED_URL
    return f"{parts.scheme}://{host}{port}/{REDACTED}"


#: Secret shapes that survive the URL reduction because they appear as bare
#: text: Hugging Face access tokens, JWT-shaped bearer material, an HTTP
#: authorization scheme and its credential, and any `name: value` pair whose
#: name says the value is a credential.
SECRET_PATTERNS = (
    (re.compile(r"\bhf_[A-Za-z0-9_]{4,}"), REDACTED_TOKEN),
    (
        re.compile(r"\beyJ[A-Za-z0-9_-]{4,}\.[A-Za-z0-9_-]{4,}\.[A-Za-z0-9_-]*"),
        REDACTED_TOKEN,
    ),
    (
        re.compile(r"(?i)\b(bearer|basic)\s+[A-Za-z0-9._~+/=-]+"),
        r"\1 " + REDACTED_TOKEN,
    ),
    (
        re.compile(
            r"(?i)\b([A-Za-z0-9_-]*"
            r"(?:authorization|token|secret|password|passwd|signature|credential"
            r"|api[_-]?key)"
            r"[A-Za-z0-9_-]*)\s*[:=]\s*[^\s,;)\]}\"']+"
        ),
        r"\1=" + REDACTED,
    ),
)


def sanitized_text(value, limit=MESSAGE_LIMIT):
    """One bounded, single-line, credential-free rendering of `value`.

    URLs are reduced first so that a secret carried inside one is removed with
    it rather than having to be recognized on its own. Anything unprintable is
    replaced before whitespace is collapsed, so a message can neither span
    lines nor forge a log record, and the result is truncated so that it cannot
    flood one.
    """
    text = "" if value is None else str(value)
    text = URL_PATTERN.sub(redacted_url, text)
    for pattern, replacement in SECRET_PATTERNS:
        text = pattern.sub(replacement, text)
    text = "".join(character if character.isprintable() else " " for character in text)
    text = " ".join(text.split())
    if len(text) > limit:
        text = text[:limit].rstrip() + TRUNCATION_MARKER
    return text


def failure_class(error):
    """The class of failure `error` is, as the helper can establish it.

    `GatedRepoError` is checked before `RepositoryNotFoundError` because the
    official client makes the former a subclass of the latter: a gated
    repository is an authorization answer, not a missing one. A status code is
    only consulted when the client attached a response, and an exception the
    helper cannot place degrades to `CLASS_CLIENT_FAILURE` rather than to a
    guess.
    """
    if isinstance(error, GatedRepoError):
        return CLASS_UNAUTHORIZED
    if isinstance(
        error, (RepositoryNotFoundError, RevisionNotFoundError, EntryNotFoundError)
    ):
        return CLASS_NOT_FOUND
    status = getattr(getattr(error, "response", None), "status_code", None)
    if (
        isinstance(error, HfHubHTTPError)
        and isinstance(status, int)
        and not isinstance(status, bool)
    ):
        if status == 404:
            return CLASS_NOT_FOUND
        if status in (401, 403):
            return CLASS_UNAUTHORIZED
        if status == 429:
            return CLASS_RATE_LIMITED
        if 500 <= status <= 599:
            return CLASS_UNAVAILABLE
        return CLASS_CLIENT_FAILURE
    if isinstance(error, (ConnectionError, TimeoutError)):
        return CLASS_UNAVAILABLE
    return CLASS_CLIENT_FAILURE


def failure_message(error, depth=CAUSE_DEPTH):
    """The sanitized reason, including the immediate cause when there is one.

    Each link is named by its exception type so that an unclassified failure
    still says what raised it, which is the difference between a reason and a
    blank.
    """
    parts = []
    seen = set()
    current = error
    while current is not None and len(parts) < depth:
        if id(current) in seen:
            break
        seen.add(id(current))
        name = sanitized_text(type(current).__name__, EXCEPTION_NAME_LIMIT)
        detail = sanitized_text(current)
        parts.append(f"{name}: {detail}" if detail else name)
        current = current.__cause__ or current.__context__
    return sanitized_text(" caused by ".join(parts))


def failure_event(error):
    return {
        "type": "failure",
        "version": FAILURE_EVENT_VERSION,
        "class": failure_class(error),
        "exception": sanitized_text(type(error).__name__, EXCEPTION_NAME_LIMIT),
        "message": failure_message(error),
    }


def report_failure(error, stream=None):
    """Reports `error` on the protocol channel and answers its exit code.

    Writing to stdout is deliberate: it is the channel the parent drains, so a
    failure report can never block on a pipe nobody is reading, and it is the
    only channel the parent keeps.
    """
    event = failure_event(error)
    print(
        json.dumps(event, separators=(",", ":")),
        file=stream or sys.stdout,
        flush=True,
    )
    return EXIT_CODES[event["class"]]


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


def repository_file_metadata(info):
    """Every file upstream reports at this commit, with its per-file metadata.

    Deliberately unfiltered by the selection: the selection narrows what this
    acquisition transfers, while this list is a property of the commit, which is
    immutable. ModelKeep records it so that it can answer repository metadata
    without acquiring the repository (Issue 0074) and so that a partially
    archived revision stops presenting its subset as the whole repository.

    `size` is the byte length, `blob_id` the git object id upstream serves as
    the `ETag` of a non-LFS file, and `lfs_sha256` the LFS object digest it
    serves as `x-linked-etag`. A field upstream did not report is omitted rather
    than guessed.
    """
    result = []
    for sibling in getattr(info, "siblings", None) or []:
        path = getattr(sibling, "rfilename", None)
        if not isinstance(path, str) or not path or ".cache" in Path(path).parts:
            continue
        if Path(path).parts[0].startswith(".modelkeep-"):
            continue
        entry = {"path": path}
        size = getattr(sibling, "size", None)
        if isinstance(size, int) and not isinstance(size, bool) and size >= 0:
            entry["size"] = size
        blob_id = getattr(sibling, "blob_id", None)
        if isinstance(blob_id, str) and blob_id:
            entry["blob_id"] = blob_id
        lfs = getattr(sibling, "lfs", None)
        lfs_sha256 = getattr(lfs, "sha256", None) if lfs is not None else None
        if isinstance(lfs_sha256, str) and lfs_sha256:
            entry["lfs_sha256"] = lfs_sha256
        result.append(entry)
    return sorted(result, key=lambda entry: entry["path"])


def expected_files(info, patterns=None, exclude=None):
    return [
        (entry["path"], entry.get("size"))
        for entry in repository_file_metadata(info)
        if selected(entry["path"], patterns, exclude)
    ]


def resolve(
    repo_id,
    requested_revision,
    files=None,
    exclude=None,
    repo_type="model",
    api=None,
    progress=None,
):
    """Resolves the commit, the files the selection covers, and the commit's
    whole upstream file list.

    Transfers nothing. The `resolved` event is emitted here so that both the
    acquisition and the resolve-only mode announce the immutable commit the same
    way. One `repo_info` call answers all three, so recording the file list
    costs no extra upstream round trip.
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
    return commit, expected_files(info, files, exclude), repository_file_metadata(info)


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

    `repository_files` is the commit's whole upstream file list, which the
    selection does not narrow: it is what ModelKeep records for the commit and
    answers metadata from.
    """
    commit, expected, repository = resolve(
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
        "repository_files": repository,
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

    commit, expected, repository = resolve(
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
    # `files` is what this acquisition archived; `repository_files` is what
    # upstream holds at the commit, which the selection does not narrow.
    return {
        "commit": commit,
        "files": archived_files,
        "repository_files": repository,
    }


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
    # Every failure is reported before it becomes an exit code (Issue 0084).
    # `SystemExit` and `KeyboardInterrupt` are deliberately not caught: argparse
    # exits through the former, and a cancellation is the parent stopping this
    # process, which is an interruption rather than an acquisition failure.
    except Exception as error:
        sys.exit(report_failure(error))
