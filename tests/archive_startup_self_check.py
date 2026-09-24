#!/usr/bin/env python3
"""Black-box archive self-consistency self-check against the packaged binary.

Every assertion here runs the ModelKeep binary the Nix package builds, not a
development shell, so the check is proven to ship with the deployed artefact.
"""

import contextlib
import hashlib
import json
import os
import shutil
import socket
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request
from pathlib import Path

HEALTHY_COMMIT = "a" * 40
DAMAGED_COMMIT = "b" * 40
ABSENT_COMMIT = "c" * 40
MANIFEST = ".modelkeep-manifest.json"
ADMIN_TOKEN = "self-check-fixture-token"
# Large enough that the reported duration is a measurement rather than noise,
# small enough to keep the check itself cheap.
SCALE_REVISIONS = 400


def free_port():
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        return listener.getsockname()[1]


def offline_environment():
    """Environment in which no upstream is reachable."""
    environment = os.environ.copy()
    environment.pop("MODELKEEP_HF_PYTHON", None)
    environment.pop("MODELKEEP_HF_HELPER", None)
    closed = f"http://127.0.0.1:{free_port()}"
    environment["HF_ENDPOINT"] = closed
    environment["http_proxy"] = closed
    environment["https_proxy"] = closed
    environment["HF_HUB_OFFLINE"] = "1"
    return environment


def write_cache_revision(cache, repo_directory, commit, files):
    repository = cache / repo_directory
    snapshot = repository / "snapshots" / commit
    snapshot.mkdir(parents=True, exist_ok=True)
    blobs = repository / "blobs"
    blobs.mkdir(parents=True, exist_ok=True)
    for name, payload in files.items():
        blob = blobs / f"{commit}-{name.replace('/', '-')}"
        blob.write_bytes(payload)
        target = snapshot / name
        target.parent.mkdir(parents=True, exist_ok=True)
        target.symlink_to(blob)


def write_cache_ref(cache, repo_directory, reference, commit):
    refs = cache / repo_directory / "refs"
    refs.mkdir(parents=True, exist_ok=True)
    (refs / reference).write_text(f"{commit}\n")


def build_archive(binary, root, revisions=0):
    """Imports a healthy fixture archive through the packaged binary."""
    cache = root / "cache"
    archive = root / "archive"
    write_cache_revision(
        cache,
        "models--org--model",
        HEALTHY_COMMIT,
        {"config.json": b"{}", "weights/model.bin": b"payload-bytes"},
    )
    write_cache_ref(cache, "models--org--model", "main", HEALTHY_COMMIT)
    write_cache_revision(
        cache, "datasets--org--data", HEALTHY_COMMIT, {"data.json": b"[]"}
    )
    write_cache_ref(cache, "datasets--org--data", "main", HEALTHY_COMMIT)
    for index in range(revisions):
        write_cache_revision(
            cache,
            "models--org--many",
            f"{index:040x}",
            {"config.json": f"{{\"index\":{index}}}".encode()},
        )
    subprocess.run(
        [str(binary), "import-hf-cache", str(cache), str(archive)],
        check=True,
        stdout=subprocess.DEVNULL,
    )
    return archive


def self_check(binary, archive, environment=None):
    completed = subprocess.run(
        [str(binary), "self-check", str(archive)],
        check=False,
        capture_output=True,
        text=True,
        env=environment if environment is not None else offline_environment(),
    )
    assert completed.stdout.strip(), completed.stderr
    return completed, json.loads(completed.stdout)


def findings_by_kind(report):
    counts = {}
    for finding in report["findings"]:
        counts[finding["finding"]] = counts.get(finding["finding"], 0) + 1
    return counts


def tree_digest(root, skip=()):
    """Content digest of a directory tree, including names, links and bytes."""
    entries = []
    for path in sorted(root.rglob("*")):
        relative = path.relative_to(root).as_posix()
        if any(relative == name or relative.startswith(f"{name}/") for name in skip):
            continue
        if path.is_symlink():
            entries.append(f"{relative}\0link:{os.readlink(path)}")
        elif path.is_dir():
            entries.append(f"{relative}\0dir")
        else:
            entries.append(
                f"{relative}\0{hashlib.sha256(path.read_bytes()).hexdigest()}"
            )
    return hashlib.sha256("\n".join(entries).encode()).hexdigest()


def manifest_path(archive, repo_type="models", repo_id="org/model", commit=None):
    return (
        archive
        / repo_type
        / repo_id
        / "revisions"
        / (commit or HEALTHY_COMMIT)
        / MANIFEST
    )


def damage_missing_file(archive):
    (archive / "models/org/model/revisions" / HEALTHY_COMMIT / "weights/model.bin").unlink()


def damage_size_mismatch(archive):
    (
        archive / "models/org/model/revisions" / HEALTHY_COMMIT / "config.json"
    ).write_bytes(b"{\"grown\":true}")


def damage_dangling_ref(archive):
    (archive / "models/org/model/refs/main").write_text(ABSENT_COMMIT)


def damage_unsafe_path(archive):
    path = manifest_path(archive)
    path.write_text(
        path.read_text().replace('"path":"config.json"', '"path":"../escape.json"')
    )


def damage_invalid_manifest(archive):
    manifest_path(archive).write_text("{ not json")


def damage_orphaned_staging(archive):
    staging = archive / "tmp" / "fetch-abandoned-left-over"
    staging.mkdir(parents=True)
    (staging / ".modelkeep-fetch.json").write_text(
        json.dumps(
            {
                "version": 1,
                "repo_type": "model",
                "repo_id": "org/model",
                "requested_revision": "main",
                "files": [],
                "resolved_commit": HEALTHY_COMMIT,
            }
        )
    )
    (staging / ".modelkeep-staging-lease").write_text(
        "nonce=abandoned\npid=1\nexpires_at=0\n"
    )


RETAINED_PARTIAL = b"retained-partial-bytes"


def leave_killed_active_staging(archive, name, repo_id, expires_at):
    """Staging a force-stopped acquisition leaves behind, exactly as it lies.

    The name is dot-prefixed, as the deployment's was: `ls` and `du -hs tmp/*` do
    not list it, which is why the self-check's count was the only hint it existed.
    """
    staging = archive / "tmp" / name
    staging.mkdir(parents=True)
    (staging / "partial.bin").write_bytes(RETAINED_PARTIAL)
    (staging / ".modelkeep-fetch.json").write_text(
        json.dumps(
            {
                "version": 1,
                "repo_type": "model",
                "repo_id": repo_id,
                "requested_revision": "main",
                "files": [],
                "resolved_commit": HEALTHY_COMMIT,
            }
        )
    )
    (staging / ".modelkeep-staging-lease").write_text(
        f"nonce=killed\npid=1\nexpires_at={expires_at}\n"
    )
    return staging


DAMAGE_CLASSES = (
    ("missing_file", damage_missing_file, {"missing_file": 1}),
    ("size_mismatch", damage_size_mismatch, {"size_mismatch": 1}),
    ("dangling_ref", damage_dangling_ref, {"dangling_ref": 1}),
    ("unsafe_path", damage_unsafe_path, {"unsafe_path": 1}),
    # An unusable manifest also makes every ref that names the revision unresolvable.
    (
        "invalid_manifest",
        damage_invalid_manifest,
        {"invalid_manifest": 1, "dangling_ref": 1},
    ),
    ("orphaned_staging", damage_orphaned_staging, {"orphaned_staging": 1}),
)


def check_healthy_archive_reports_zero_findings(binary, root):
    archive = build_archive(binary, root / "healthy")
    completed, report = self_check(binary, archive)

    assert completed.returncode == 0, completed.stderr
    assert report["status"] == "clean", report
    assert report["findings"] == [], report
    assert report["repositories_checked"] == 2, report
    assert report["revisions_checked"] == 2, report
    assert report["files_checked"] == 3, report
    assert report["refs_checked"] == 2, report
    assert report["orphaned_staging_directories"] == 0, report
    assert report["completed_at"] > 0, report
    # A zero-findings result is still a result: "checked and clean" is not the
    # same answer as "never checked".
    assert isinstance(report["duration_ms"], int), report
    print(f"healthy: clean in {report['duration_ms']} ms")


def check_each_damage_class_is_detected(binary, root):
    for name, damage, expected in DAMAGE_CLASSES:
        archive = build_archive(binary, root / f"damage-{name}")
        damage(archive)
        before = tree_digest(archive)

        completed, report = self_check(binary, archive)

        assert completed.returncode != 0, (name, completed.stdout)
        assert report["status"] == "findings", (name, report)
        assert findings_by_kind(report) == expected, (name, report)
        if name == "orphaned_staging":
            assert report["orphaned_staging_directories"] == 1, report
            assert report["findings"][0]["age_seconds"] is not None, report

        # Nothing is repaired, deleted, or re-acquired: the archive is
        # byte-for-byte unchanged and a second run says exactly the same thing.
        assert tree_digest(archive) == before, name
        _, repeated = self_check(binary, archive)
        assert findings_by_kind(repeated) == expected, (name, repeated)
        assert tree_digest(archive) == before, name
        print(f"{name}: detected, archive unchanged")


def check_internal_paths_are_counted_rather_than_reported(binary, root):
    """An old manifest listing ModelKeep's own metadata is not a finding."""
    archive = build_archive(binary, root / "internal-path")
    path = manifest_path(archive)
    path.write_text(
        path.read_text().replace(
            '"path":"config.json"', '"path":".cache/huggingface/download.json"'
        )
    )

    completed, report = self_check(binary, archive)

    assert completed.returncode == 0, completed.stderr
    assert report["status"] == "clean", report
    assert report["filtered_internal_paths"] == 1, report
    # Serving filters the path, so the check never repeats it either.
    assert ".cache/huggingface" not in completed.stdout, completed.stdout
    assert ".cache/huggingface" not in completed.stderr, completed.stderr


def check_published_revisions_are_untouched(binary, root):
    archive = build_archive(binary, root / "immutable")
    before = tree_digest(archive)

    completed, report = self_check(binary, archive)

    assert completed.returncode == 0, completed.stderr
    assert report["findings"] == [], report
    assert tree_digest(archive) == before


def check_runs_with_upstream_unreachable(binary, root):
    archive = build_archive(binary, root / "offline")
    environment = offline_environment()
    # Nothing in the check may reach for upstream, so a helper that could not
    # possibly run must not change the outcome.
    environment["MODELKEEP_HF_PYTHON"] = "/nonexistent/python3"
    environment["MODELKEEP_HF_HELPER"] = "/nonexistent/hf_fetch.py"

    completed, report = self_check(binary, archive, environment)

    assert completed.returncode == 0, completed.stderr
    assert report["status"] == "clean", report
    assert report["revisions_checked"] == 2, report


def check_duration_is_measured_against_revision_count(binary, root):
    archive = build_archive(binary, root / "scale", revisions=SCALE_REVISIONS)

    _, small = self_check(binary, build_archive(binary, root / "scale-small"))
    _, large = self_check(binary, archive)

    assert large["revisions_checked"] == SCALE_REVISIONS + 2, large
    assert large["files_checked"] == SCALE_REVISIONS + 3, large
    assert large["findings"] == [], large
    print(
        f"duration: {small['duration_ms']} ms for {small['revisions_checked']} "
        f"revisions, {large['duration_ms']} ms for {large['revisions_checked']} "
        "revisions"
    )
    return archive, large["duration_ms"]


def start_server(binary, archive, log):
    port = free_port()
    admin_port = free_port()
    environment = offline_environment()
    environment["MODELKEEP_ADMIN_ADDRESS"] = f"127.0.0.1:{admin_port}"
    environment["MODELKEEP_ADMIN_TOKEN"] = ADMIN_TOKEN
    environment.pop("MODELKEEP_TRUST_TAILSCALE_HEADERS", None)
    process = subprocess.Popen(
        [str(binary), "serve", str(archive), f"127.0.0.1:{port}"],
        env=environment,
        stdout=subprocess.DEVNULL,
        stderr=log,
        start_new_session=True,
    )
    endpoint = f"http://127.0.0.1:{port}"
    for _ in range(400):
        if process.poll() is not None:
            raise AssertionError("ModelKeep exited during startup")
        try:
            with urllib.request.urlopen(f"{endpoint}/readyz", timeout=0.2) as response:
                if response.status == 200:
                    return process, endpoint, f"http://127.0.0.1:{admin_port}"
        except OSError:
            time.sleep(0.025)
    process.kill()
    raise AssertionError("ModelKeep did not become ready")


@contextlib.contextmanager
def server(binary, archive, log):
    process, endpoint, admin = start_server(binary, archive, log)
    try:
        yield endpoint, admin
    finally:
        process.terminate()
        with contextlib.suppress(subprocess.TimeoutExpired):
            process.wait(timeout=10)
        process.kill()


def admin_status(admin):
    request = urllib.request.Request(
        f"{admin}/api/admin/v1/status",
        headers={"Authorization": f"Bearer {ADMIN_TOKEN}"},
    )
    with urllib.request.urlopen(request, timeout=5) as response:
        return json.loads(response.read())


def admin_call(admin, path, method="GET", csrf=False, token=ADMIN_TOKEN):
    """One management request, returning its status and decoded body.

    A refusal is an answer here, not an exception: every authorization, CSRF and
    unsafe-name assertion below is about the status code the service chose.
    """
    headers = {}
    if token is not None:
        headers["Authorization"] = f"Bearer {token}"
    if csrf:
        headers["X-ModelKeep-CSRF"] = "1"
    request = urllib.request.Request(
        f"{admin}{path}", headers=headers, method=method
    )
    try:
        with urllib.request.urlopen(request, timeout=120) as response:
            return response.status, json.loads(response.read())
    except urllib.error.HTTPError as error:
        body = error.read()
        return error.code, json.loads(body) if body else {}


def trigger_self_check(admin):
    """Runs the self-check through the API and returns its fresh result."""
    deadline = time.monotonic() + 120
    while time.monotonic() < deadline:
        status, report = admin_call(
            admin, "/api/admin/v1/self-check", method="POST", csrf=True
        )
        # The startup check may still be in flight; a second walk of the same
        # archive is refused rather than duplicated.
        if status == 409:
            time.sleep(0.05)
            continue
        assert status == 200, (status, report)
        return report
    raise AssertionError("the self-check never became triggerable")


def leave_retained_staging(archive, name, expires_at, commit, payload):
    """Staging the temporary area retains, with its own measurable bytes.

    Returns the directory and the total byte/file count of everything written
    into it, so the listing's measurement is compared against an independent
    count rather than against a second copy of the walk it proves.
    """
    staging = archive / "tmp" / name
    (staging / "nested").mkdir(parents=True)
    written = []
    target = staging / "nested" / "partial.bin"
    target.write_bytes(payload)
    written.append(payload)
    if expires_at is not None:
        lease = f"nonce=fixture\npid=1\nexpires_at={expires_at}\n".encode()
        (staging / ".modelkeep-staging-lease").write_bytes(lease)
        written.append(lease)
    if commit is not None:
        identity = json.dumps(
            {
                "version": 1,
                "repo_type": "model",
                "repo_id": "org/model",
                "requested_revision": "main",
                "files": [],
                "resolved_commit": commit,
            }
        ).encode()
        (staging / ".modelkeep-fetch.json").write_bytes(identity)
        written.append(identity)
    return staging, sum(len(entry) for entry in written), len(written)


def check_startup_reports_findings_without_delaying_serving(binary, archive, log_path):
    """The check runs at startup, beside serving, and only reports."""
    damage_missing_file(archive)
    damage_orphaned_staging(archive)
    # `tmp` belongs to recovery and `state` to the management control plane, so
    # this compares exactly the published revisions and refs the check reads.
    before = tree_digest(archive, skip=("tmp", "state"))

    with log_path.open("wb") as log:
        with server(binary, archive, log) as (endpoint, admin):
            # Readiness was already reached above, so serving does not wait for
            # the check. A healthy revision is servable throughout.
            started = time.monotonic()
            with urllib.request.urlopen(
                f"{endpoint}/org/model/resolve/main/config.json", timeout=5
            ) as response:
                assert response.status == 200
                assert response.read() == b"{}"
            served_in = time.monotonic() - started
            assert served_in < 5, served_in

            observed = []
            deadline = time.monotonic() + 60
            while time.monotonic() < deadline:
                summary = admin_status(admin)["self_check"]
                observed.append(summary["status"])
                if summary["status"] in ("clean", "findings"):
                    break
                time.sleep(0.02)
            else:
                raise AssertionError(f"self-check never completed: {observed}")

    assert summary["status"] == "findings", summary
    assert summary["finding_count"] == 2, summary
    assert summary["findings_by_kind"] == {
        "missing_file": 1,
        "orphaned_staging": 1,
    }, summary
    assert summary["revisions_checked"] == SCALE_REVISIONS + 2, summary
    assert summary["orphaned_staging_directories"] == 1, summary
    assert summary["completed_at"] > 0, summary
    assert isinstance(summary["duration_ms"], int), summary

    # Startup reported and repaired nothing: the damaged revision and the
    # staging left behind are both still exactly as they were.
    assert tree_digest(archive, skip=("tmp", "state")) == before
    assert (archive / "tmp/fetch-abandoned-left-over/.modelkeep-fetch.json").is_file()

    events = [
        json.loads(line)["fields"]
        for line in log_path.read_text(errors="replace").splitlines()
        if line.startswith("{")
    ]
    names = [event.get("event") for event in events]
    assert "archive_self_check_completed" in names, names
    # One restart must cost one result line at the default level, so the
    # per-check "started" event stays below it.
    assert "archive_self_check_started" not in names, names
    completed = next(
        event for event in events if event.get("event") == "archive_self_check_completed"
    )
    assert completed["status"] == "findings", completed
    assert completed["finding_count"] == 2, completed
    assert completed["revisions_checked"] == SCALE_REVISIONS + 2, completed
    findings = [
        event for event in events if event.get("event") == "archive_self_check_finding"
    ]
    assert sorted(event["finding"] for event in findings) == [
        "missing_file",
        "orphaned_staging",
    ], findings
    print(
        "startup: served during the check, reported "
        f"{completed['finding_count']} findings in {completed['duration_ms']} ms "
        f"over {completed['revisions_checked']} revisions"
    )


def check_startup_reclaims_expired_active_staging(binary, root, log_path):
    """Issue 0083: startup recovery reclaims a dead marker and says which it did.

    Two markers are left behind: one whose lease expired, which recovery must
    rename into the adoptable form with its bytes intact, and one whose lease is
    still live, which recovery must not touch at all.
    """
    archive = build_archive(binary, root / "reclaim")
    expired = leave_killed_active_staging(
        archive,
        # The deployment's name, which `du -hs tmp/*` did not list.
        ".fetch-active-4bb1baf7182d415883bc6d0576a909d3",
        "org/model",
        0,
    )
    live = leave_killed_active_staging(
        archive, ".fetch-active-live", "org/data", int(time.time()) + 3600
    )

    with log_path.open("wb") as log:
        with server(binary, archive, log) as (endpoint, _admin):
            with urllib.request.urlopen(f"{endpoint}/readyz", timeout=5) as response:
                assert response.status == 200

    events = [
        json.loads(line)["fields"]
        for line in log_path.read_text(errors="replace").splitlines()
        if line.startswith("{")
    ]
    recovered = [
        event for event in events if event.get("event") == "incomplete_fetch_recovered"
    ]
    assert len(recovered) == 1, recovered
    assert recovered[0]["recovery_action"] == "preserved_for_resume", recovered
    assert recovered[0]["repo_id"] == "org/model", recovered
    assert recovered[0]["requested_revision"] == "main", recovered
    assert recovered[0]["commit"] == HEALTHY_COMMIT, recovered

    # Renamed into the adoptable form with its retained bytes intact: unblocking
    # an acquisition never deletes what a resume would reuse (ADR-0017).
    assert not expired.exists(), "the expired marker was left in place"
    adoptable = sorted((archive / "tmp").glob("fetch-abandoned-*"))
    assert len(adoptable) == 1, adoptable
    assert (adoptable[0] / "partial.bin").read_bytes() == RETAINED_PARTIAL
    # A live lease is a running acquisition, whatever else recovery finds.
    assert live.is_dir(), "recovery reclaimed staging whose lease was still live"
    assert (live / "partial.bin").read_bytes() == RETAINED_PARTIAL
    print("startup recovery: expired marker renamed for resume, live lease untouched")


def block_removal(path):
    """Makes `path` unremovable by this process, and proves that it is.

    Dropping write permission on a directory stops its entries from being
    unlinked, which is what removing the directory has to do first. That has no
    effect for a caller privileged enough to ignore it, so the premise is
    asserted rather than assumed (Issue 0087): a check that silently does not
    reach the failure path would be worse than no check, because it would assert
    the fix without proving it.
    """
    path.chmod(0o555)
    try:
        shutil.rmtree(path)
    except PermissionError:
        assert (
            path / ".modelkeep-staging-lease"
        ).is_file(), "the refused removal destroyed part of the fixture"
        return
    except OSError as error:
        raise AssertionError(
            "removal was refused for a reason other than the permissions this "
            f"fixture set, so the premise is not the one it set up: {error!r}"
        ) from error
    raise AssertionError(
        f"removal of {path} could not be blocked in this environment, so this "
        "check did not exercise the failure path it asserts; it has to run as a "
        "uid that ordinary directory permissions apply to"
    )


def check_startup_isolates_an_unremovable_staging_entry(binary, root, log_path):
    """Issue 0087: junk in a scratch directory cannot take the mirror offline.

    Three entries are left under the staging directory: one the runtime user
    cannot remove, one it can, and one identified download it must rename for a
    later resume. The unremovable one must cost exactly itself.
    """
    archive = build_archive(binary, root / "stuck")
    removable = archive / "tmp" / "stuck-removable"
    removable.mkdir(parents=True)
    (removable / "partial.bin").write_bytes(RETAINED_PARTIAL)
    (removable / ".modelkeep-staging-lease").write_text(
        "nonce=dead\npid=1\nexpires_at=0\n"
    )
    resumable = leave_killed_active_staging(
        archive, ".fetch-active-stuck-neighbour", "org/model", 0
    )
    blocked = archive / "tmp" / "stuck-blocked"
    blocked.mkdir(parents=True)
    (blocked / ".modelkeep-staging-lease").write_text(
        "nonce=stuck\npid=1\nexpires_at=0\n"
    )
    before = tree_digest(archive, skip=("tmp", "state"))
    block_removal(blocked)
    adoptable = []

    try:
        with log_path.open("wb") as log:
            # Startup reaching readiness at all is the first assertion: under
            # `restart: unless-stopped` an abort here is a crash loop.
            with server(binary, archive, log) as (endpoint, admin):
                with urllib.request.urlopen(
                    f"{endpoint}/org/model/resolve/main/config.json", timeout=5
                ) as response:
                    assert response.status == 200
                    assert response.read() == b"{}"

                adoptable.extend(sorted((archive / "tmp").glob("fetch-abandoned-*")))
                assert len(adoptable) == 1, adoptable

                # Issue 0081: what recovery could not reclaim and what the
                # self-check counts are reconciled from the API alone. The
                # listing names the entry, says it is the one recovery skipped,
                # and repeats the action and failure kind the
                # `staging_recovery_skipped` event reported.
                status, listing = admin_call(admin, "/api/admin/v1/staging")
                assert status == 200, listing
                entries = {item["name"]: item for item in listing["items"]}
                stuck = entries["stuck-blocked"]
                assert stuck["retention"] == "not_reclaimable", stuck
                assert stuck["recovery_skipped_action"] == "discard", stuck
                assert stuck["recovery_skipped_io_kind"] == "permission_denied", stuck
                assert stuck["adoptable"] is False, stuck
                kept = entries[adoptable[0].name]
                assert kept["retention"] == "resumable", kept
                assert kept["adoptable"] is True, kept
                assert kept["commit"] == HEALTHY_COMMIT, kept
                assert kept["size_bytes"] >= len(RETAINED_PARTIAL), kept
                assert listing["retained_by_kind"] == {
                    "not_reclaimable": 1,
                    "resumable": 1,
                }, listing

                # The self-check's own findings are readable through the API,
                # and they name the same directories the listing does.
                report = trigger_self_check(admin)
                counted = sorted(
                    finding["path"]
                    for finding in report["findings"]
                    if finding["finding"] == "orphaned_staging"
                )
                assert counted == sorted(entries), (counted, sorted(entries))
                assert report["orphaned_staging_directories"] == 2, report

        events = [
            json.loads(line)["fields"]
            for line in log_path.read_text(errors="replace").splitlines()
            if line.startswith("{")
        ]
        names = [event.get("event") for event in events]
        assert "archive_recovery_completed" in names, names
        assert "archive_recovery_failed" not in names, names
        assert "process_failed" not in names, names
        completed = next(
            event for event in events if event.get("event") == "archive_recovery_completed"
        )
        assert completed["recovered_staging_directories"] == 1, completed

        skipped = [
            event for event in events if event.get("event") == "staging_recovery_skipped"
        ]
        assert len(skipped) == 1, skipped
        assert skipped[0]["staging"] == "stuck-blocked", skipped
        assert skipped[0]["error_class"] == "recovery_skipped", skipped
        assert skipped[0]["recovery_action"] == "discard", skipped
        assert skipped[0]["io_kind"] == "permission_denied", skipped
        # The entry is named, the archive path it sits under is not.
        assert str(archive) not in json.dumps(skipped[0]), skipped

        # Every other reclaimable entry was still reclaimed, by both paths
        # recovery has: the one it discards and the one it renames for resume.
        assert not removable.exists(), "a reclaimable entry survived recovery"
        assert not resumable.exists(), "an identified download was not renamed"
        assert len(adoptable) == 1, adoptable
        assert (adoptable[0] / "partial.bin").read_bytes() == RETAINED_PARTIAL

        # The entry that could not be reclaimed is left exactly as it was: this
        # changes error handling, not reclamation policy (ADR-0009). No partial
        # data became observable, and no published revision was touched.
        assert blocked.is_dir()
        assert (blocked / ".modelkeep-staging-lease").is_file()
        assert tree_digest(archive, skip=("tmp", "state")) == before

        # The startup self-check still counts what recovery could not reclaim.
        _, report = self_check(binary, archive)
        orphaned = sorted(
            finding["path"]
            for finding in report["findings"]
            if finding["finding"] == "orphaned_staging"
        )
        assert "stuck-blocked" in orphaned, report
        assert orphaned == sorted(["stuck-blocked", adoptable[0].name]), report
        assert report["orphaned_staging_directories"] == 2, report
    finally:
        # Restore write permission so the fixture can be cleaned up.
        blocked.chmod(0o755)
    print(
        "startup: skipped one unremovable staging entry, reclaimed the rest, "
        "and served throughout"
    )


def check_operator_acts_on_retained_staging_through_the_api(binary, root, log_path):
    """Issue 0081: the detector's findings are actionable without `rm -rf`.

    Three retained shapes are left behind, one of each kind startup leaves for an
    operator, and every step of the procedure the runbook documents is performed
    through the management API against the packaged binary: list, judge, refuse
    what must not be removed, remove one, and re-verify.
    """
    archive = build_archive(binary, root / "operator")
    resumable, resumable_bytes, resumable_files = leave_retained_staging(
        archive,
        "fetch-abandoned-operator",
        0,
        HEALTHY_COMMIT,
        b"resumable-partial-bytes" * 64,
    )
    live, _live_bytes, _live_files = leave_retained_staging(
        archive,
        ".fetch-active-operator-live",
        int(time.time()) + 3600,
        HEALTHY_COMMIT,
        b"in-flight-bytes",
    )
    unreadable, unreadable_bytes, unreadable_files = leave_retained_staging(
        archive, "fetch-abandoned-unreadable", None, HEALTHY_COMMIT, b"kept-for-inspection"
    )
    revision = archive / "models/org/model/revisions" / HEALTHY_COMMIT
    before = tree_digest(archive, skip=("tmp", "state"))

    with log_path.open("wb") as log:
        with server(binary, archive, log) as (endpoint, admin):
            # 1. What is retained, why, and how much each one holds.
            status, listing = admin_call(admin, "/api/admin/v1/staging")
            assert status == 200, listing
            entries = {item["name"]: item for item in listing["items"]}
            assert sorted(entries) == sorted(
                [
                    ".fetch-active-operator-live",
                    "fetch-abandoned-operator",
                    "fetch-abandoned-unreadable",
                ]
            ), listing
            assert listing["retained_by_kind"] == {
                "active": 1,
                "resumable": 1,
                "unreadable_lease": 1,
            }, listing

            kept = entries["fetch-abandoned-operator"]
            assert kept["retention"] == "resumable", kept
            assert kept["adoptable"] is True, kept
            assert kept["removable"] is True, kept
            assert kept["repo_type"] == "model", kept
            assert kept["repo_id"] == "org/model", kept
            assert kept["requested_revision"] == "main", kept
            assert kept["commit"] == HEALTHY_COMMIT, kept
            assert kept["selection"] == [], kept
            # The size is the staging directory's own, which is the figure Issue
            # 0082 needs; the archive filesystem's free space is not it.
            assert kept["size_bytes"] == resumable_bytes, kept
            assert kept["file_count"] == resumable_files, kept
            assert kept["size_complete"] is True, kept
            assert kept["age_seconds"] >= 0, kept
            assert kept["lease_expires_in_seconds"] == 0, kept

            inspect = entries["fetch-abandoned-unreadable"]
            assert inspect["retention"] == "unreadable_lease", inspect
            assert inspect["adoptable"] is False, inspect
            assert inspect["size_bytes"] == unreadable_bytes, inspect
            assert inspect["file_count"] == unreadable_files, inspect

            running = entries[".fetch-active-operator-live"]
            assert running["retention"] == "active", running
            assert running["removable"] is False, running
            assert running["lease_expires_in_seconds"] > 0, running

            # 2. The management plane's own rules apply to the removal.
            assert (
                admin_call(
                    admin,
                    "/api/admin/v1/staging/fetch-abandoned-operator",
                    method="DELETE",
                    csrf=True,
                    token=None,
                )[0]
                == 401
            )
            assert (
                admin_call(
                    admin,
                    "/api/admin/v1/staging/fetch-abandoned-operator",
                    method="DELETE",
                )[0]
                == 403
            )

            # 3. A live acquisition's staging is not removable, and the refusal
            #    says how long the lease still has.
            status, refused = admin_call(
                admin,
                "/api/admin/v1/staging/.fetch-active-operator-live",
                method="DELETE",
                csrf=True,
            )
            assert status == 409, refused
            assert refused["error"] == "staging_active", refused
            assert refused["lease_expires_in_seconds"] > 0, refused
            assert live.is_dir(), "a live acquisition's staging was removed"

            # 4. No name reaches out of `tmp`, including one naming a published
            #    revision that removal would otherwise destroy.
            for name in (
                f"..%2Fmodels%2Forg%2Fmodel%2Frevisions%2F{HEALTHY_COMMIT}",
                "..%2F..%2Fmodels",
                "..",
                "nested%2Fpath",
            ):
                status, body = admin_call(
                    admin, f"/api/admin/v1/staging/{name}", method="DELETE", csrf=True
                )
                assert status == 400, (name, status, body)
                assert body["error"] == "invalid_request", (name, body)
            assert revision.is_dir(), "a rejected name reached a published revision"

            # 5. One named directory is removed, and nothing published moves.
            status, removed = admin_call(
                admin,
                "/api/admin/v1/staging/fetch-abandoned-operator",
                method="DELETE",
                csrf=True,
            )
            assert status == 200, removed
            assert removed["name"] == "fetch-abandoned-operator", removed
            assert removed["retention"] == "resumable", removed
            assert removed["size_bytes"] == resumable_bytes, removed
            assert not resumable.exists()
            assert tree_digest(archive, skip=("tmp", "state")) == before
            with urllib.request.urlopen(
                f"{endpoint}/org/model/resolve/main/config.json", timeout=5
            ) as response:
                assert response.status == 200
                assert response.read() == b"{}"

            assert (
                admin_call(
                    admin,
                    "/api/admin/v1/staging/fetch-abandoned-operator",
                    method="DELETE",
                    csrf=True,
                )[0]
                == 404
            )

            # 6. Re-verification is a request, not a restart: the fresh result
            #    no longer counts what was removed, and the status route then
            #    reports that same fresh result.
            report = trigger_self_check(admin)
            counted = sorted(
                finding["path"]
                for finding in report["findings"]
                if finding["finding"] == "orphaned_staging"
            )
            assert counted == ["fetch-abandoned-unreadable"], report
            assert report["orphaned_staging_directories"] == 1, report
            assert report["staging_directories"] == 2, report
            summary = admin_status(admin)["self_check"]
            assert summary["orphaned_staging_directories"] == 1, summary
            assert summary["findings_by_kind"] == {"orphaned_staging": 1}, summary

            assert unreadable.is_dir(), "an entry nobody named was removed"
    print(
        "operator: listed three retention kinds, refused a live lease and every "
        "unsafe name, removed one directory and re-verified through the API"
    )


def main():
    binary = Path(sys.argv[1])
    with tempfile.TemporaryDirectory() as temporary:
        root = Path(temporary)
        check_healthy_archive_reports_zero_findings(binary, root)
        check_each_damage_class_is_detected(binary, root)
        check_internal_paths_are_counted_rather_than_reported(binary, root)
        check_published_revisions_are_untouched(binary, root)
        check_runs_with_upstream_unreachable(binary, root)
        check_startup_reclaims_expired_active_staging(
            binary, root, root / "reclaim.log"
        )
        check_startup_isolates_an_unremovable_staging_entry(
            binary, root, root / "stuck.log"
        )
        check_operator_acts_on_retained_staging_through_the_api(
            binary, root, root / "operator.log"
        )
        archive, _ = check_duration_is_measured_against_revision_count(binary, root)
        check_startup_reports_findings_without_delaying_serving(
            binary, archive, root / "serve.log"
        )
        # The fixtures are large; drop them explicitly so cleanup is cheap.
        shutil.rmtree(root / "scale", ignore_errors=True)


if __name__ == "__main__":
    main()
