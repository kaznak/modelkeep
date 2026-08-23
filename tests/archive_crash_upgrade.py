#!/usr/bin/env python3
"""Black-box process crash and released-writer/current-reader archive checks."""

import contextlib
import hashlib
import os
import signal
import socket
import subprocess
import sys
import tempfile
import threading
import time
import urllib.error
import urllib.request
from pathlib import Path


STABLE_COMMIT = "b" * 40


def free_port():
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        return listener.getsockname()[1]


def start_server(binary, archive, helper=None):
    port = free_port()
    endpoint = f"http://127.0.0.1:{port}"
    environment = os.environ.copy()
    if helper is None:
        environment.pop("MODELKEEP_HF_PYTHON", None)
        environment.pop("MODELKEEP_HF_HELPER", None)
    else:
        environment["MODELKEEP_HF_PYTHON"] = sys.executable
        environment["MODELKEEP_HF_HELPER"] = str(helper)
    process = subprocess.Popen(
        [str(binary), "serve", str(archive), f"127.0.0.1:{port}"],
        env=environment,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.PIPE,
        start_new_session=True,
    )
    for _ in range(200):
        if process.poll() is not None:
            raise AssertionError(process.stderr.read().decode(errors="replace"))
        try:
            with urllib.request.urlopen(f"{endpoint}/readyz", timeout=0.2) as response:
                if response.status == 200:
                    return process, endpoint
        except OSError:
            time.sleep(0.025)
    stop_server(process)
    raise AssertionError("ModelKeep did not become ready")


def stop_server(process, force=False):
    if process.poll() is None:
        os.killpg(process.pid, signal.SIGKILL if force else signal.SIGTERM)
    process.wait(timeout=5)


@contextlib.contextmanager
def server(binary, archive, helper=None):
    process, endpoint = start_server(binary, archive, helper)
    try:
        yield endpoint
    finally:
        stop_server(process)


def create_cache(root, repo, commit, payload):
    repository = root / "cache" / f"models--{repo.replace('/', '--')}"
    blob = repository / "blobs" / "config"
    snapshot = repository / "snapshots" / commit
    blob.parent.mkdir(parents=True)
    snapshot.mkdir(parents=True)
    blob.write_bytes(payload)
    (snapshot / "config.json").symlink_to(blob)
    (repository / "refs").mkdir()
    (repository / "refs" / "main").write_text(commit)
    return root / "cache"


def import_cache(binary, cache, archive):
    subprocess.run(
        [str(binary), "import-hf-cache", str(cache), str(archive)],
        check=True,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )


def response_status(url):
    try:
        with urllib.request.urlopen(url, timeout=2) as response:
            response.read()
            return response.status
    except urllib.error.HTTPError as error:
        return error.code


def tree_digests(root):
    return {
        path.relative_to(root).as_posix(): hashlib.sha256(path.read_bytes()).hexdigest()
        for path in sorted(root.rglob("*"))
        if path.is_file()
    }


def expire_staging_leases(archive):
    staging = [path for path in (archive / "tmp").iterdir() if path.is_dir()]
    assert staging, "crashed acquisition left no staging directory"
    for directory in staging:
        lease = directory / ".modelkeep-staging-lease"
        lines = lease.read_text().splitlines()
        lease.write_text(
            "\n".join(
                "expires_at=0" if line.startswith("expires_at=") else line
                for line in lines
            )
            + "\n"
        )
    return staging


def crash_recovery_check(current, crash_helper, root):
    archive = root / "crash-archive"
    cache = create_cache(root / "stable", "org/stable", STABLE_COMMIT, b"stable")
    import_cache(current, cache, archive)

    process, endpoint = start_server(current, archive, crash_helper)
    request_error = []

    def request_missing_revision():
        try:
            urllib.request.urlopen(
                f"{endpoint}/api/models/org/crash/revision/main", timeout=30
            ).read()
        except Exception as error:  # The connection must fail when the process dies.
            request_error.append(error)

    requester = threading.Thread(target=request_missing_revision)
    requester.start()
    for _ in range(200):
        if list((archive / "tmp").glob("fetch-*/partial.bin")):
            break
        time.sleep(0.025)
    else:
        stop_server(process, force=True)
        raise AssertionError("fetch fixture did not write its partial payload")
    stop_server(process, force=True)
    requester.join(timeout=5)
    assert not requester.is_alive()
    assert request_error

    revisions = archive / "models" / "org" / "crash" / "revisions"
    assert not revisions.exists() or not list(
        revisions.iterdir()
    ), "partial revision was published before process death"
    with server(current, archive) as offline:
        assert response_status(f"{offline}/api/models/org/crash/revision/main") == 404
        with urllib.request.urlopen(
            f"{offline}/org/stable/resolve/{STABLE_COMMIT}/config.json"
        ) as response:
            assert response.read() == b"stable"

    staging = expire_staging_leases(archive)
    with server(current, archive) as offline:
        assert response_status(f"{offline}/readyz") == 200
    assert all(not path.exists() for path in staging)
    subprocess.run(
        [str(current), "verify", str(archive), "org/stable", STABLE_COMMIT],
        check=True,
        stdout=subprocess.DEVNULL,
    )


def upgrade_check(old, current, root):
    archive = root / "upgrade-archive"
    payload = b"archive-created-by-v0.2.1"
    cache = create_cache(root / "old", "org/upgrade", STABLE_COMMIT, payload)
    import_cache(old, cache, archive)
    models = archive / "models"
    before = tree_digests(models)

    subprocess.run(
        [str(current), "verify", str(archive), "org/upgrade", STABLE_COMMIT],
        check=True,
        stdout=subprocess.DEVNULL,
    )
    with server(current, archive) as endpoint:
        with urllib.request.urlopen(
            f"{endpoint}/org/upgrade/resolve/{STABLE_COMMIT}/config.json"
        ) as response:
            assert response.read() == payload
        with urllib.request.urlopen(
            f"{endpoint}/org/upgrade/resolve/main/config.json"
        ) as response:
            assert response.read() == payload

    assert tree_digests(models) == before, "current reader modified the old archive"


def main():
    if len(sys.argv) != 4:
        raise SystemExit("usage: archive_crash_upgrade.py CURRENT OLD CRASH_HELPER")
    current, old, crash_helper = map(Path, sys.argv[1:])
    with tempfile.TemporaryDirectory() as temporary:
        root = Path(temporary)
        crash_recovery_check(current, crash_helper, root)
        upgrade_check(old, current, root)


if __name__ == "__main__":
    main()
