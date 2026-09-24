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


def start_admin_server(binary, archive, helper, extra_environment=None, log=None):
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
    environment.update(extra_environment or {})
    process = subprocess.Popen(
        [str(binary), "serve", str(archive), f"127.0.0.1:{download_port}"],
        env=environment,
        stdout=subprocess.DEVNULL,
        stderr=log if log is not None else subprocess.PIPE,
        start_new_session=True,
    )
    for _ in range(200):
        if process.poll() is not None:
            detail = (
                process.stderr.read().decode(errors="replace")
                if process.stderr is not None
                else "see the server log"
            )
            raise AssertionError(detail)
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


KILLED_REPO = "org/killed"
KILLED_COMMIT = "d" * 40
# Big enough that "half of it" is an unambiguous measurement, small enough that
# the check stays cheap.
KILLED_PAYLOAD = b"m" * 65536
KILLED_RETAINED = len(KILLED_PAYLOAD) // 2

# Generated into the harness's temporary directory rather than shipped under
# tests/fixtures/, because the Nix check passes fixture paths to this script as
# fixed positional arguments.
KILLED_HELPER_SOURCE = '''#!/usr/bin/env python3
"""Fixture that stalls mid-transfer once, then continues from retained bytes.

Every invocation appends what it actually transferred to FIXTURE_TRANSFER_LOG, so
a resumed acquisition's cost is measured rather than assumed.
"""

import argparse
import json
import os
import time
from pathlib import Path

COMMIT = "{commit}"
PAYLOAD = b"m" * {size}
RETAINED = len(PAYLOAD) // 2

parser = argparse.ArgumentParser()
parser.add_argument("--repo-id", required=True)
parser.add_argument("--repo-type", choices=("model", "dataset"), default="model")
parser.add_argument("--revision", required=True)
parser.add_argument("--output")
parser.add_argument("--file", action="append")
parser.add_argument("--exclude", action="append")
parser.add_argument("--resolve-only", action="store_true", dest="resolve_only")
args = parser.parse_args()

result = {{"type": "result", "commit": COMMIT, "files": ["model.bin"]}}

if args.resolve_only:
    print(json.dumps(result, separators=(",", ":")), flush=True)
    raise SystemExit(0)

if not args.output:
    parser.error("--output is required unless --resolve-only is given")

output = Path(args.output)
output.mkdir(parents=True, exist_ok=True)
target = output / "model.bin"
retained = target.read_bytes() if target.is_file() else b""
if retained and not PAYLOAD.startswith(retained):
    raise SystemExit("staging held bytes this fixture never wrote")


def record(transferred):
    with open(os.environ["FIXTURE_TRANSFER_LOG"], "a") as log:
        log.write(
            json.dumps(
                {{
                    "repo_id": args.repo_id,
                    "revision": args.revision,
                    "retained": len(retained),
                    "transferred": transferred,
                }},
                separators=(",", ":"),
            )
            + "\\n"
        )


print(json.dumps({{"type": "resolved", "version": 1, "commit": COMMIT}}), flush=True)

if os.environ.get("FIXTURE_STALL") == "1":
    # Transfer part of the payload and then stall, so the harness can force-stop
    # the process while the acquisition is genuinely in flight.
    target.write_bytes(PAYLOAD[:RETAINED])
    record(RETAINED - len(retained))
    print(
        json.dumps(
            {{
                "type": "progress",
                "phase": "downloading",
                "unit": "bytes",
                "completed": RETAINED,
                "total": len(PAYLOAD),
            }}
        ),
        flush=True,
    )
    time.sleep(300)

target.write_bytes(PAYLOAD)
record(len(PAYLOAD) - len(retained))
print(json.dumps(result, separators=(",", ":")), flush=True)
'''


def write_killed_helper(path):
    path.write_text(
        KILLED_HELPER_SOURCE.format(commit=KILLED_COMMIT, size=len(KILLED_PAYLOAD))
    )
    return path


def transfer_records(log):
    if not log.exists():
        return []
    return [json.loads(line) for line in log.read_text().splitlines() if line.strip()]


def admin_request(admin_endpoint, path, data=None, method="GET", idempotency=None):
    headers = {"Authorization": "Bearer fixture-token"}
    if data is not None:
        headers["Content-Type"] = "application/json"
        headers["X-ModelKeep-CSRF"] = "1"
    if idempotency is not None:
        headers["Idempotency-Key"] = idempotency
    request = urllib.request.Request(
        f"{admin_endpoint}/api/admin/v1/{path}",
        data=json.dumps(data).encode() if data is not None else None,
        headers=headers,
        method=method,
    )
    with urllib.request.urlopen(request, timeout=10) as response:
        return response.status, json.loads(response.read())


def submit_prefetch(admin_endpoint, idempotency, repo_id=KILLED_REPO):
    status, job = admin_request(
        admin_endpoint,
        "jobs",
        data={
            "kind": "prefetch",
            "repo_type": "model",
            "repo_id": repo_id,
            "revision": "main",
        },
        method="POST",
        idempotency=idempotency,
    )
    assert status == 202, (status, job)
    return job["id"]


def await_terminal_job(admin_endpoint, job_id, timeout=120):
    deadline = time.monotonic() + timeout
    job = None
    while time.monotonic() < deadline:
        _, job = admin_request(admin_endpoint, f"jobs/{job_id}")
        if job["state"] in ("completed", "failed", "cancelled"):
            return job
        time.sleep(0.05)
    raise AssertionError(f"job never reached a terminal state: {job}")


def server_events(log_path):
    return [
        json.loads(line)["fields"]
        for line in log_path.read_text(errors="replace").splitlines()
        if line.startswith("{")
    ]


def killed_staging_checkpoint(archive):
    """The active marker a killed acquisition would leave, once it is resumable."""
    states = []
    for staging in sorted((archive / "tmp").glob(".fetch-active-*")):
        payload = staging / "model.bin"
        size = payload.stat().st_size if payload.is_file() else None
        try:
            metadata = json.loads((staging / ".modelkeep-fetch.json").read_text())
        except (OSError, json.JSONDecodeError) as error:
            states.append(f"{staging.name}: bytes={size}, metadata={error!r}")
            continue
        states.append(
            f"{staging.name}: bytes={size}, resolved_commit={metadata.get('resolved_commit')!r}"
        )
        if size == KILLED_RETAINED and metadata.get("resolved_commit") == KILLED_COMMIT:
            return staging, states
    return None, states


def await_killed_staging(process, archive):
    for _ in range(400):
        staging, states = killed_staging_checkpoint(archive)
        if staging is not None:
            return staging
        time.sleep(0.025)
    stop_server(process, force=True)
    raise AssertionError(
        "the acquisition never reached a resumable checkpoint: "
        + ("; ".join(states) or "no active fetch staging")
    )


def expire_lease(staging):
    """Model the lease expiring, which is the passage of time and nothing else.

    The directory keeps its `.fetch-active-` name: renaming it by hand is exactly
    the manual step Issue 0083 exists to remove.
    """
    lease = staging / ".modelkeep-staging-lease"
    lines = lease.read_text().splitlines()
    assert any(line.startswith("expires_at=") for line in lines), lines
    lease.write_text(
        "\n".join(
            "expires_at=0" if line.startswith("expires_at=") else line for line in lines
        )
        + "\n"
    )


def killed_acquisition_resume_check(current, root):
    """Issue 0083: force-stop, then resubmit the same identity, twice over.

    `crash_recovery_check` above recreates the container *and* expires every
    staging lease by hand before restarting, so it never exercised what a bare
    force-stop leaves: an active marker whose lease is still live, then expired,
    with nothing renaming it in between.
    """
    archive = root / "killed-archive"
    helper = write_killed_helper(root / "killed-helper.py")
    transfers = root / "killed-transfers.jsonl"
    environment = {"FIXTURE_TRANSFER_LOG": str(transfers)}

    # 1. An acquisition is force-stopped while it is transferring.
    process, _, admin = start_admin_server(
        current, archive, helper, {**environment, "FIXTURE_STALL": "1"}
    )
    submit_prefetch(admin, "killed-first")
    killed = await_killed_staging(process, archive)
    stop_server(process, force=True)

    first = transfer_records(transfers)
    assert [record["transferred"] for record in first] == [KILLED_RETAINED], first
    assert (killed / "model.bin").stat().st_size == KILLED_RETAINED
    assert killed.name.startswith(".fetch-active-"), killed.name

    # 2. The container comes back while the dead process's lease is still live,
    #    the same identity is resubmitted, and then the lease expires *while the
    #    server keeps running*. That is the deployment's sequence, and it is the
    #    one no restart can paper over: startup recovery already ran, with the
    #    lease live, so nothing but the acquisition itself can release the marker.
    #    The whole sequence is therefore one server session, and the absence of
    #    `incomplete_fetch_recovered` from its log is part of the assertion.
    session_log = root / "killed-session.log"
    with session_log.open("wb") as log:
        process, download, admin = start_admin_server(
            current, archive, helper, environment, log
        )
        try:
            assert killed.is_dir(), "recovery reclaimed staging whose lease was live"
            refused = await_terminal_job(admin, submit_prefetch(admin, "killed-live"))
            assert refused["state"] == "failed", refused
            assert "publication" not in (refused["message"] or ""), refused
            assert killed.is_dir(), "a refused acquisition removed the staging"
            assert (
                transfer_records(transfers) == first
            ), "a refused acquisition transferred bytes"

            # 3. The lease expires. Nothing is renamed, and nothing restarts.
            expire_lease(killed)

            # 4. The next acquisition of the same identity adopts the bytes.
            resumed = await_terminal_job(
                admin, submit_prefetch(admin, "killed-resumed")
            )
            with urllib.request.urlopen(
                f"{download}/{KILLED_REPO}/resolve/{KILLED_COMMIT}/model.bin", timeout=30
            ) as response:
                assert response.read() == KILLED_PAYLOAD
        finally:
            stop_server(process)

    events = server_events(session_log)
    names = [event.get("event") for event in events]
    assert "incomplete_fetch_recovered" not in names, names
    conflicts = [event for event in events if event.get("event") == "fetch_staging_conflict"]
    assert conflicts, names
    assert conflicts[0]["error_class"] == "staging_conflict", conflicts
    assert 0 < conflicts[0]["lease_expires_in_seconds"] <= 120, conflicts
    assert conflicts[0]["repo_id"] == KILLED_REPO, conflicts

    assert resumed["state"] == "completed", resumed
    assert resumed["resumed"] is True, resumed
    assert resumed["resolved_commit"] == KILLED_COMMIT, resumed
    resumed_transfer = transfer_records(transfers)[-1]
    assert resumed_transfer["retained"] == KILLED_RETAINED, resumed_transfer

    # 5. Measure the same work from nothing, in an archive holding no staging.
    fresh_archive = root / "fresh-archive"
    process, _, admin = start_admin_server(current, fresh_archive, helper, environment)
    try:
        fresh = await_terminal_job(admin, submit_prefetch(admin, "killed-fresh"))
    finally:
        stop_server(process)

    assert fresh["state"] == "completed", fresh
    assert fresh["resumed"] is False, fresh
    fresh_transfer = transfer_records(transfers)[-1]
    assert fresh_transfer["retained"] == 0, fresh_transfer
    assert resumed_transfer["transferred"] < fresh_transfer["transferred"], (
        resumed_transfer,
        fresh_transfer,
    )
    print(
        "killed acquisition: resume moved "
        f"{resumed_transfer['transferred']} bytes against "
        f"{fresh_transfer['transferred']} from a fresh start, after reusing "
        f"{resumed_transfer['retained']} retained bytes with no manual rename"
    )


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
        killed_acquisition_resume_check(current, root)
        upgrade_check(old, current, root)


if __name__ == "__main__":
    main()
