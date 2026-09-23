#!/usr/bin/env python3
"""Black-box process crash and released-writer/current-reader archive checks."""

import contextlib
import hashlib
import json
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
CRASH_COMMIT = "c" * 40
PARTIAL_PAYLOAD = b"incomplete-model-payload"


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


def start_admin_server(binary, archive, helper):
    download_port = free_port()
    admin_port = free_port()
    download_endpoint = f"http://127.0.0.1:{download_port}"
    admin_endpoint = f"http://127.0.0.1:{admin_port}"
    environment = os.environ.copy()
    environment["MODELKEEP_HF_PYTHON"] = sys.executable
    environment["MODELKEEP_HF_HELPER"] = str(helper)
    environment["MODELKEEP_ADMIN_ADDRESS"] = f"127.0.0.1:{admin_port}"
    environment["MODELKEEP_ADMIN_TOKEN"] = "fixture-token"
    environment.pop("MODELKEEP_TRUST_TAILSCALE_HEADERS", None)
    process = subprocess.Popen(
        [str(binary), "serve", str(archive), f"127.0.0.1:{download_port}"],
        env=environment,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.PIPE,
        start_new_session=True,
    )
    for _ in range(200):
        if process.poll() is not None:
            raise AssertionError(process.stderr.read().decode(errors="replace"))
        request = urllib.request.Request(
            f"{admin_endpoint}/api/admin/v1/status",
            headers={"Authorization": "Bearer fixture-token"},
        )
        try:
            with urllib.request.urlopen(request, timeout=0.2) as response:
                if response.status == 200:
                    return process, download_endpoint, admin_endpoint
        except OSError:
            time.sleep(0.025)
    stop_server(process, force=True)
    raise AssertionError("ModelKeep admin API did not become ready")


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


def resumable_checkpoint(archive):
    """Return durable resumable staging, plus state useful on timeout."""
    states = []
    for staging in sorted((archive / "tmp").glob(".fetch-active-*")):
        partial = staging / "partial.bin"
        metadata_path = staging / ".modelkeep-fetch.json"
        payload_ready = partial.is_file() and partial.read_bytes() == PARTIAL_PAYLOAD
        try:
            metadata = json.loads(metadata_path.read_text())
        except (OSError, json.JSONDecodeError) as error:
            states.append(f"{staging.name}: payload={payload_ready}, metadata={error!r}")
            continue
        commit = metadata.get("resolved_commit")
        states.append(
            f"{staging.name}: payload={payload_ready}, resolved_commit={commit!r}"
        )
        if (
            payload_ready
            and metadata.get("repo_id") == "org/crash"
            and metadata.get("requested_revision") == "main"
            and commit == CRASH_COMMIT
        ):
            return staging, states
    return None, states


def crash_recovery_check(current, crash_helper, resume_helper, root):
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
    checkpoint_states = []
    for _ in range(200):
        checkpoint, checkpoint_states = resumable_checkpoint(archive)
        if checkpoint is not None:
            break
        time.sleep(0.025)
    else:
        stop_server(process, force=True)
        raise AssertionError(
            "fetch fixture did not reach its durable resumable checkpoint: "
            + ("; ".join(checkpoint_states) or "no active fetch staging")
        )
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
    with server(current, archive, resume_helper) as resumed:
        with urllib.request.urlopen(
            f"{resumed}/org/crash/resolve/main/partial.bin", timeout=10
        ) as response:
            assert response.read() == b"incomplete-model-payload-resumed"
    assert all(not path.exists() for path in staging)
    subprocess.run(
        [str(current), "verify", str(archive), "org/stable", STABLE_COMMIT],
        check=True,
        stdout=subprocess.DEVNULL,
    )


def active_prefetch_shutdown_check(current, crash_helper, root):
    archive = root / "shutdown-archive"
    process, _, admin_endpoint = start_admin_server(current, archive, crash_helper)
    request = urllib.request.Request(
        f"{admin_endpoint}/api/admin/v1/jobs",
        data=json.dumps(
            {
                "kind": "prefetch",
                "repo_type": "model",
                "repo_id": "org/crash",
                "revision": "main",
            }
        ).encode(),
        headers={
            "Authorization": "Bearer fixture-token",
            "Content-Type": "application/json",
            "X-ModelKeep-CSRF": "1",
            "Idempotency-Key": "active-prefetch-shutdown",
        },
        method="POST",
    )
    with urllib.request.urlopen(request, timeout=2) as response:
        assert response.status == 202

    checkpoint_states = []
    for _ in range(200):
        checkpoint, checkpoint_states = resumable_checkpoint(archive)
        if checkpoint is not None:
            break
        time.sleep(0.025)
    else:
        stop_server(process, force=True)
        raise AssertionError(
            "admin prefetch did not reach its resumable checkpoint: "
            + ("; ".join(checkpoint_states) or "no active fetch staging")
        )

    # Match a container runtime sending SIGTERM only to PID 1. The process must not
    # wait for the detached long-running prefetch worker.
    process.terminate()
    try:
        process.wait(timeout=5)
    finally:
        # The helper shares the test process group and may outlive the parent outside
        # a container PID namespace. Clean it up without weakening the exit assertion.
        with contextlib.suppress(ProcessLookupError):
            os.killpg(process.pid, signal.SIGKILL)
    assert process.returncode == 0


def upgrade_check(old, current, root):
    archive = root / "upgrade-archive"
    payload = b"archive-created-by-v0.2.1"
    cache = create_cache(root / "old", "org/upgrade", STABLE_COMMIT, payload)
    import_cache(old, cache, archive)
    models = archive / "models"
    before = tree_digests(models)
    sentinel = archive / "operator-sentinel"
    sentinel.write_bytes(b"must-remain-unchanged")
    legacy_tree = tree_digests(archive)

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
    assert (archive / "datasets").is_dir(), "upgrade did not add the dataset namespace"
    assert not any((archive / "datasets").iterdir()), "upgrade populated the dataset namespace"
    expected_upgrade_tree = dict(legacy_tree)
    assert tree_digests(archive) == expected_upgrade_tree, "upgrade modified legacy archive files"

    downgrade_check(old, archive, payload)


def downgrade_check(old, archive, model_payload):
    """A pre-dataset reader must ignore, and never modify, the dataset namespace."""
    commit = STABLE_COMMIT
    revision = archive / "datasets" / "org" / "upgrade" / "revisions" / commit
    revision.mkdir(parents=True)
    dataset_payload = b"dataset-must-not-be-served-as-model"
    (revision / "config.json").write_bytes(dataset_payload)
    manifest = {
        "version": 1,
        "complete": True,
        "repo_type": "dataset",
        "repo_id": "org/upgrade",
        "requested_revision": "main",
        "commit": commit,
        "archived_at": 0,
        "files": [
            {
                "path": "config.json",
                "size": len(dataset_payload),
                "sha256": hashlib.sha256(dataset_payload).hexdigest(),
            }
        ],
    }
    (revision / ".modelkeep-manifest.json").write_text(
        json.dumps(manifest, separators=(",", ":")) + "\n"
    )
    refs = archive / "datasets" / "org" / "upgrade" / "refs"
    refs.mkdir()
    (refs / "main").write_text(commit)
    before = tree_digests(archive)

    with server(old, archive) as endpoint:
        with urllib.request.urlopen(
            f"{endpoint}/org/upgrade/resolve/{commit}/config.json"
        ) as response:
            assert response.read() == model_payload
        assert response_status(
            f"{endpoint}/api/datasets/org/upgrade/revision/{commit}"
        ) == 404

    assert tree_digests(archive) == before, "pre-dataset binary modified archive contents"


def main():
    if len(sys.argv) != 5:
        raise SystemExit("usage: archive_crash_upgrade.py CURRENT OLD CRASH_HELPER RESUME_HELPER")
    current, old, crash_helper, resume_helper = map(Path, sys.argv[1:])
    with tempfile.TemporaryDirectory() as temporary:
        root = Path(temporary)
        active_prefetch_shutdown_check(current, crash_helper, root)
        crash_recovery_check(current, crash_helper, resume_helper, root)
        upgrade_check(old, current, root)


if __name__ == "__main__":
    main()
