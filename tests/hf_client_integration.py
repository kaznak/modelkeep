#!/usr/bin/env python3
import contextlib
import hashlib
import importlib.util
import json
import os
import socket
import subprocess
import sys
import tempfile
import threading
import time
import urllib.error
import urllib.request
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path

from huggingface_hub import HfApi, hf_hub_download, snapshot_download
from huggingface_hub import __version__ as huggingface_hub_version
from huggingface_hub.errors import HfHubHTTPError


COMMIT = "a" * 40
REPO_ID = "org/model"
COLLISION_REPO_ID = "org/shards"
COLD_MISS_REPO_ID = "org/cold-miss"
METADATA_REPO_ID = "org/metadata-wait"
ADMIN_TOKEN = "modelkeep-hf-client-integration-admin-token"

# What the model fixture holds at COMMIT. The mirror must report all of it for a
# revision it has never seen, and archive only what the client asks for.
UPSTREAM_FILES = [
    "README.md",
    "config.json",
    "model-00001-of-00002.safetensors",
    "model-00002-of-00002.safetensors",
    "model.safetensors",
    "model.safetensors.index.json",
    "tokenizer.json",
]

# An upstream helper that records every invocation and delays the transfer, so a
# real client meets ModelKeep's cold-miss deadline (Issue 0069) instead of the
# stub used by the Rust unit tests. The payload itself is delegated to the same
# fixture the rest of this test uses, so the acquisition contract stays in one
# place; only timing and an invocation journal are added.
SLOW_UPSTREAM_HELPER = r'''#!/usr/bin/env python3
import json
import os
import runpy
import sys
import time
from pathlib import Path

with Path(os.environ["MODELKEEP_SLOW_HELPER_JOURNAL"]).open("a") as journal:
    journal.write(json.dumps(sys.argv[1:]) + "\n")
if "--resolve-only" not in sys.argv:
    time.sleep(float(os.environ["MODELKEEP_SLOW_HELPER_DELAY"]))
runpy.run_path(os.environ["MODELKEEP_SLOW_HELPER_FIXTURE"], run_name="__main__")
'''


def unused_port():
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        return listener.getsockname()[1]


@contextlib.contextmanager
def server(
    binary,
    archive,
    helper=None,
    captured_logs=None,
    upstream_endpoint=None,
    admin_endpoints=None,
    extra_environment=None,
):
    port = unused_port()
    endpoint = f"http://127.0.0.1:{port}"
    environment = os.environ.copy()
    if helper:
        environment["MODELKEEP_HF_PYTHON"] = sys.executable
        environment["MODELKEEP_HF_HELPER"] = str(helper)
    else:
        environment.pop("MODELKEEP_HF_PYTHON", None)
        environment.pop("MODELKEEP_HF_HELPER", None)
    if upstream_endpoint:
        environment["HF_ENDPOINT"] = upstream_endpoint
    else:
        environment.pop("HF_ENDPOINT", None)
    environment.pop("MODELKEEP_ADMIN_ADDRESS", None)
    environment.pop("MODELKEEP_ADMIN_TOKEN", None)
    if admin_endpoints is not None:
        admin_port = unused_port()
        environment["MODELKEEP_ADMIN_ADDRESS"] = f"127.0.0.1:{admin_port}"
        environment["MODELKEEP_ADMIN_TOKEN"] = ADMIN_TOKEN
        admin_endpoints.append(f"http://127.0.0.1:{admin_port}")
    environment.pop("MODELKEEP_COLD_MISS_DEADLINE_SECONDS", None)
    if extra_environment:
        environment.update(extra_environment)
    process = subprocess.Popen(
        [str(binary), "serve", str(archive), f"127.0.0.1:{port}"],
        env=environment,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.PIPE,
        text=True,
    )
    # Drain stderr continuously. Reading it only at teardown lets the pipe fill
    # and blocks the server on its next log write, which deadlocks the test for
    # reasons that have nothing to do with what it asserts.
    drained = []
    drain = threading.Thread(
        target=lambda stream: drained.extend(iter(stream.readline, "")),
        args=(process.stderr,),
        daemon=True,
    )
    drain.start()

    def logs():
        return "".join(drained)

    try:
        for _ in range(100):
            if process.poll() is not None:
                drain.join(timeout=5)
                raise RuntimeError(logs())
            try:
                with urllib.request.urlopen(f"{endpoint}/readyz", timeout=0.2) as response:
                    if response.status == 200:
                        break
            except OSError:
                time.sleep(0.05)
        else:
            raise RuntimeError("ModelKeep did not become ready")
        yield endpoint
    finally:
        process.terminate()
        process.wait(timeout=5)
        drain.join(timeout=5)
        if captured_logs is not None:
            captured_logs.append(logs())


@contextlib.contextmanager
def localhost_only():
    original_connect = socket.socket.connect

    def checked_connect(sock, address):
        host = address[0]
        if host not in ("127.0.0.1", "::1", "localhost"):
            raise AssertionError(f"client attempted mirror bypass to {host}")
        return original_connect(sock, address)

    socket.socket.connect = checked_connect
    try:
        yield
    finally:
        socket.socket.connect = original_connect


def download(endpoint, destination, revision, repo_type="model"):
    return snapshot_download(
        repo_id=REPO_ID,
        repo_type=repo_type,
        revision=revision,
        endpoint=endpoint,
        local_dir=str(destination),
    )


def admin_call(admin_endpoint, path, body=None, idempotency_key=None):
    request = urllib.request.Request(
        f"{admin_endpoint}{path}",
        method="GET" if body is None else "POST",
        data=None if body is None else json.dumps(body).encode(),
    )
    request.add_header("Authorization", f"Bearer {ADMIN_TOKEN}")
    if body is not None:
        request.add_header("Content-Type", "application/json")
        request.add_header("X-ModelKeep-CSRF", "1")
        request.add_header("Idempotency-Key", idempotency_key)
    with urllib.request.urlopen(request, timeout=60) as response:
        return json.loads(response.read())


def await_job(admin_endpoint, job_id, timeout=180):
    limit = time.monotonic() + timeout
    while True:
        job = admin_call(admin_endpoint, f"/api/admin/v1/jobs/{job_id}")
        if job["state"] not in ("queued", "running"):
            return job
        assert time.monotonic() < limit, f"job did not finish: {job}"
        time.sleep(0.25)


def assert_cold_miss_deadline_is_retried_by_the_client(binary, fixture, root):
    """Issue 0069, black-box against a real supported client.

    The Rust unit tests pin the bounded `503` against an in-process stub. This
    pins what the client actually does with it: a cold-miss `resolve` whose
    acquisition outlasts `MODELKEEP_COLD_MISS_DEADLINE_SECONDS` is retried by
    the client itself and completes, and the retry joins the running flight
    instead of starting a second upstream transfer. The metadata route is answered
    from upstream's file list without acquiring anything (Issue 0074), so the same
    slow acquisition must neither delay nor fail a client there.
    """
    journal = root / "cold-miss-helper-journal.jsonl"
    journal.write_text("")
    slow_helper = root / "slow-upstream-helper.py"
    slow_helper.write_text(SLOW_UPSTREAM_HELPER)
    logs = []
    with server(
        binary,
        root / "cold-miss-archive",
        slow_helper,
        logs,
        extra_environment={
            "MODELKEEP_COLD_MISS_DEADLINE_SECONDS": "1",
            "MODELKEEP_SLOW_HELPER_JOURNAL": str(journal),
            "MODELKEEP_SLOW_HELPER_DELAY": "3",
            "MODELKEEP_SLOW_HELPER_FIXTURE": str(fixture),
        },
    ) as endpoint:
        downloaded = Path(
            hf_hub_download(
                repo_id=COLD_MISS_REPO_ID,
                filename="config.json",
                revision="main",
                endpoint=endpoint,
                local_dir=str(root / "cold-miss-client"),
            )
        )
        assert downloaded.read_bytes() == b'{"model_type":"modelkeep-fixture"}'

        # The metadata route answers from upstream's file list without waiting
        # for any acquisition (Issue 0074), so the slow helper never delays it.
        metadata = HfApi(endpoint=endpoint).repo_info(METADATA_REPO_ID, revision="main")
        assert metadata.sha == COMMIT
        assert sorted(
            sibling.rfilename for sibling in metadata.siblings
        ) == UPSTREAM_FILES, metadata.siblings

    # The deadline really was reached, so the client saw the bounded answer
    # rather than one uninterrupted wait.
    bounded = [
        line
        for line in logs[0].splitlines()
        if '"event":"acquisition_deadline_exceeded"' in line
        and f'"repo_id":"{COLD_MISS_REPO_ID}"' in line
    ]
    assert bounded, logs[0]

    # ... and the retries joined it: exactly one upstream transfer, for the one
    # requested path.
    invocations = [json.loads(line) for line in journal.read_text().splitlines()]
    transfers = [
        argv
        for argv in invocations
        if "--output" in argv and COLD_MISS_REPO_ID in argv
    ]
    assert len(transfers) == 1, invocations
    assert "config.json" in transfers[0], transfers


def assert_selected_subset_is_acquired_and_served(binary, root, actual_helper, populated):
    """Issue 0070, black-box against a real supported client.

    A filtered prefetch archives a subset of a revision; a real client then
    downloads that subset with `allow_patterns` while upstream is unavailable.
    The already populated archive acts as the local upstream, and the production
    helper performs the filtered acquisition.

    Repository metadata reports the commit's whole upstream file list, because the
    prefetch recorded it (Issue 0074). A partially archived revision therefore no
    longer presents its subset as the whole repository: an unfiltered download of
    it needs the paths the archive does not hold, and with upstream unavailable
    that fails rather than handing back an incomplete model and calling it a
    successful download.
    """
    selected_archive = root / "selection-archive"
    admin_endpoints = []
    with server(binary, populated) as upstream_endpoint:
        with server(
            binary,
            selected_archive,
            actual_helper,
            upstream_endpoint=upstream_endpoint,
            admin_endpoints=admin_endpoints,
        ):
            admin_endpoint = admin_endpoints[0]
            submitted = admin_call(
                admin_endpoint,
                "/api/admin/v1/jobs",
                body={
                    "kind": "prefetch",
                    "repo_type": "model",
                    "repo_id": REPO_ID,
                    "revision": COMMIT,
                    "include": ["config.json", "tokenizer.json"],
                },
                idempotency_key="hf-client-integration-selection",
            )
            assert submitted["include"] == ["config.json", "tokenizer.json"]
            job = await_job(admin_endpoint, submitted["id"])
            assert job["state"] == "completed", job
            assert job["outcome"] == "published", job
            assert job["total_files"] == 2, job

    # Upstream is unavailable from here on: no helper and no HF_ENDPOINT.
    with server(binary, selected_archive) as endpoint:
        info = HfApi(endpoint=endpoint).repo_info(REPO_ID, revision=COMMIT)
        assert sorted(
            sibling.rfilename for sibling in info.siblings
        ) == UPSTREAM_FILES, info.siblings
        subset = Path(
            snapshot_download(
                repo_id=REPO_ID,
                revision=COMMIT,
                endpoint=endpoint,
                local_dir=str(root / "selected-subset-client"),
                allow_patterns=["config.json"],
            )
        )
        assert (subset / "config.json").read_bytes() == (
            b'{"model_type":"modelkeep-fixture"}'
        )
        assert not (subset / "tokenizer.json").exists()
        pair = Path(
            snapshot_download(
                repo_id=REPO_ID,
                revision=COMMIT,
                endpoint=endpoint,
                local_dir=str(root / "selected-pair-client"),
                allow_patterns=["config.json", "tokenizer.json"],
            )
        )
        assert (pair / "tokenizer.json").read_bytes() == b'{"version":"1.0"}'
        # An unfiltered download asks for the shards the archive does not hold,
        # which needs upstream. It must fail rather than succeed with a model that
        # is missing shards.
        try:
            download(endpoint, root / "selected-whole-client", COMMIT)
        except Exception:
            pass
        else:
            raise AssertionError(
                "an unfiltered download of a partially archived revision reported "
                "success without the files the archive does not hold"
            )
    # Nothing was acquired by any of that: upstream was unavailable throughout.
    assert sorted(archived_digests(selected_archive, REPO_ID, COMMIT)) == [
        "config.json",
        "tokenizer.json",
    ]


def revision_directory(archive, repo_id, commit):
    namespace, repo = repo_id.split("/")
    return archive / "models" / namespace / repo / "revisions" / commit


def assert_a_client_filter_narrows_the_first_acquisition(
    binary, root, actual_helper, populated
):
    """Issue 0074 acceptance, black-box against a real supported client.

    A client's own `allow_patterns` narrows the **first** acquisition of a
    repository the mirror has never seen: repository metadata is answered from
    upstream without acquiring anything, so only the matching files are ever
    transferred and archived. Measured before this change, the same command
    archived the full snapshot, because the metadata request acquired the whole
    repository and the filter was applied to a file list ModelKeep had already
    paid for in full.

    The already populated archive acts as the local upstream and the production
    helper performs both the metadata resolve and the acquisition, so this
    exercises the real helper contract rather than a synthetic fixture.
    """
    mirror = root / "unseen-filtered-archive"
    logs = []
    with server(binary, populated) as upstream_endpoint:
        with server(
            binary,
            mirror,
            actual_helper,
            logs,
            upstream_endpoint=upstream_endpoint,
        ) as endpoint:
            # Metadata for a revision the mirror has never seen reports the whole
            # repository, and archives nothing at all.
            info = HfApi(endpoint=endpoint).repo_info(REPO_ID, revision="main")
            assert info.sha == COMMIT
            assert sorted(
                sibling.rfilename for sibling in info.siblings
            ) == UPSTREAM_FILES, info.siblings
            assert not revision_directory(mirror, REPO_ID, COMMIT).exists()

            client = Path(
                snapshot_download(
                    repo_id=REPO_ID,
                    revision="main",
                    endpoint=endpoint,
                    local_dir=str(root / "unseen-filtered-client"),
                    allow_patterns=["config.json"],
                )
            )
            assert (client / "config.json").read_bytes() == (
                b'{"model_type":"modelkeep-fixture"}'
            )
            assert not (client / "model.safetensors").exists()

    # The acceptance criterion: the archived set is the filtered set.
    archived = archived_digests(mirror, REPO_ID, COMMIT)
    assert sorted(archived) == ["config.json"], archived
    materialized = sorted(
        path.name
        for path in revision_directory(mirror, REPO_ID, COMMIT).iterdir()
        if not path.name.startswith(".modelkeep-")
    )
    assert materialized == ["config.json"], materialized

    # The commit's upstream file list was recorded as internal archive state, so
    # the partially archived revision knows what it does not hold.
    recorded = json.loads(
        (
            revision_directory(mirror, REPO_ID, COMMIT)
            / ".modelkeep-upstream-files.json"
        ).read_text()
    )
    assert recorded["commit"] == COMMIT
    assert [entry["path"] for entry in recorded["files"]] == UPSTREAM_FILES, recorded
    assert [entry.get("blob_id") for entry in recorded["files"]] == [
        upstream_blob_id(path) for path in UPSTREAM_FILES
    ], recorded
    assert '"event":"upstream_metadata_answered"' in logs[0], logs[0]

    # Warm and offline: no helper and no HF_ENDPOINT, so upstream is
    # unavailable. The revision still answers metadata, and still reports the
    # files it does not hold (core invariant 8).
    with server(binary, mirror) as endpoint:
        with urllib.request.urlopen(
            f"{endpoint}/api/models/{REPO_ID}/tree/{COMMIT}"
        ) as response:
            tree = json.loads(response.read())
        assert sorted(entry["path"] for entry in tree) == UPSTREAM_FILES, tree
        held = [entry for entry in tree if entry["path"] == "config.json"][0]
        absent = [entry for entry in tree if entry["path"] == "model.safetensors"][0]
        assert held["size"] == 34, held
        # `oid` is upstream's git object id, which travelled the whole chain: the
        # fixture reported it, the upstream mirror published it as `blob_id`, the
        # real helper read it, and this mirror recorded it (Issue 0079). It is not
        # a digest of anything ModelKeep holds.
        assert held["oid"] == upstream_blob_id("config.json"), held
        assert held["oid"] != archived["config.json"], held
        # ModelKeep's own digest lives in ModelKeep's own property, and is the
        # validator the mirror serves and verifies against.
        assert held["modelkeep"]["sha256"] == archived["config.json"], held
        status, headers, _ = resolve_headers(
            endpoint, REPO_ID, COMMIT, "config.json", method="HEAD"
        )
        assert headers["ETag"] == f'"{held["modelkeep"]["sha256"]}"', headers
        # A file the mirror does not hold: upstream's object id is reported,
        # because the commit states it, and no digest is claimed for bytes the
        # mirror does not have.
        assert absent["oid"] == upstream_blob_id("model.safetensors"), absent
        assert absent["modelkeep"]["sha256"] is None, absent


def upstream_blob_id(path):
    """The git object id the model fixture reports for a path.

    The same rule as `tests/fixtures/hf_fetch_fixture.py`: a git blob id is a
    sha1 over the blob, so it is deliberately not any digest of the content, and
    a check that compares the two catches a response reporting one under the
    other's name (Issue 0079).
    """
    return hashlib.sha1(b"blob:" + path.encode()).hexdigest()


def assert_metadata_reports_hub_fields_and_modelkeeps_digest(binary, archive, root):
    """Issue 0079 acceptance, black-box against a real supported client.

    Both metadata routes report the Hub's fields with the Hub's meanings — `oid`
    and `blob_id` are upstream's git object id, never a digest — and carry
    ModelKeep's own property beside them, whose `sha256` is the validator the
    `resolve` route serves. The bytes are verified against that property, which is
    what the documentation tells a caller to do.

    Nothing is relayed and no `lfs` object is reported: upstream recorded an LFS
    digest for the `.safetensors` files, and it reaches no response. Emitting
    `lfs` would move the validator a supported client uses off the value ModelKeep
    computes and serves, and both pinned versions reject an `lfs` object without
    `pointerSize`, which ModelKeep does not record. See
    `docs/observations/hugging-face-lfs-reporting-2026-09-24.md`.

    The mirror is offline throughout: no helper and no `HF_ENDPOINT`, so every
    answer comes from the archive.
    """
    digests = archived_digests(archive, REPO_ID, COMMIT)
    recorded = json.loads(
        (
            revision_directory(archive, REPO_ID, COMMIT)
            / ".modelkeep-upstream-files.json"
        ).read_text()
    )
    # The precondition that makes the LFS half of this check mean anything.
    assert [
        entry["path"] for entry in recorded["files"] if entry.get("lfs_sha256")
    ] == [path for path in UPSTREAM_FILES if path.endswith(".safetensors")], recorded

    with server(binary, archive) as endpoint:
        with urllib.request.urlopen(
            f"{endpoint}/api/models/{REPO_ID}/tree/{COMMIT}"
        ) as response:
            tree = json.loads(response.read())
        with urllib.request.urlopen(
            f"{endpoint}/api/models/{REPO_ID}/revision/{COMMIT}"
        ) as response:
            info = json.loads(response.read())
        siblings = {sibling["rfilename"]: sibling for sibling in info["siblings"]}
        assert sorted(entry["path"] for entry in tree) == UPSTREAM_FILES, tree
        assert sorted(siblings) == UPSTREAM_FILES, info

        for entry in tree:
            path = entry["path"]
            sibling = siblings[path]
            # No `lfs` object and no `xetHash` in either route, for an
            # LFS-managed file as much as for a plain one. A relayed LFS digest
            # cannot be distinguished from ModelKeep's own here, because upstream
            # reports the real digest of the same bytes; the divergent case is
            # pinned by the Rust test
            # `an_lfs_managed_file_reports_no_lfs_object_and_one_verifiable_digest`.
            assert "lfs" not in entry, entry
            assert "lfs" not in sibling, sibling
            assert "xetHash" not in entry, entry
            assert "pointerSize" not in json.dumps([entry, sibling]), entry
            # The Hub's fields, with the Hub's meanings, in both routes.
            assert entry["oid"] == upstream_blob_id(path), entry
            assert sibling["blobId"] == entry["oid"], sibling
            assert sibling["size"] == entry["size"], sibling
            # ModelKeep's own property, on every file the archive holds, equal in
            # both routes and equal to the validator `resolve` serves.
            digest = entry["modelkeep"]["sha256"]
            assert digest == digests[path], entry
            assert sibling["modelkeep"] == entry["modelkeep"], sibling
            assert digest != entry["oid"], entry
            status, headers, _ = resolve_headers(
                endpoint, REPO_ID, COMMIT, path, method="HEAD"
            )
            assert status == 200, (path, status)
            assert headers["ETag"] == f'"{digest}"', headers
            # The documented recipe: read the tree, fetch the bytes, verify them
            # against ModelKeep's property.
            _, _, payload = resolve_headers(
                endpoint, REPO_ID, COMMIT, path, method="GET"
            )
            assert hashlib.sha256(payload).hexdigest() == digest, path

        # Both pinned clients read the same answers without being disturbed by the
        # unknown `modelkeep` property: 1.27.0 keeps unknown top-level keys and
        # ignores unknown per-file keys, 0.36.0 ignores both. What is required here
        # is only that neither rejects them.
        api = HfApi(endpoint=endpoint)
        client_info = api.repo_info(REPO_ID, revision=COMMIT, files_metadata=True)
        assert sorted(sibling.rfilename for sibling in client_info.siblings) == (
            UPSTREAM_FILES
        ), client_info.siblings
        for sibling in client_info.siblings:
            assert sibling.lfs is None, sibling
            assert sibling.blob_id == upstream_blob_id(sibling.rfilename), sibling
            assert sibling.size == siblings[sibling.rfilename]["size"], sibling
        listed = list(api.list_repo_tree(REPO_ID, revision=COMMIT, recursive=True))
        assert sorted(item.path for item in listed) == UPSTREAM_FILES, listed
        for item in listed:
            assert item.lfs is None, item
            assert item.blob_id == upstream_blob_id(item.path), item

        client = Path(
            snapshot_download(
                repo_id=REPO_ID,
                revision=COMMIT,
                endpoint=endpoint,
                local_dir=str(root / "faithful-metadata-client"),
            )
        )
        for path, digest in digests.items():
            assert hashlib.sha256((client / path).read_bytes()).hexdigest() == (
                digest
            ), path


def load_module(path):
    specification = importlib.util.spec_from_file_location(path.stem, path)
    module = importlib.util.module_from_spec(specification)
    specification.loader.exec_module(module)
    return module


def archived_digests(archive, repo_id, commit):
    """The digests the archive itself recorded, as the comparison of record.

    Comparing a download against the fixture's own bytes would only prove the
    client and the fixture agree. The archive is the durable state a client is
    entitled to receive, so its manifest is what a downloaded file is checked
    against.
    """
    namespace, repo = repo_id.split("/")
    manifest = json.loads(
        (
            archive
            / "models"
            / namespace
            / repo
            / "revisions"
            / commit
            / ".modelkeep-manifest.json"
        ).read_text()
    )
    return {entry["path"]: entry["sha256"] for entry in manifest["files"]}


def resolve_headers(endpoint, repo_id, commit, path, method="HEAD", headers=None):
    request = urllib.request.Request(
        f"{endpoint}/{repo_id}/resolve/{commit}/{path}",
        method=method,
        headers=headers or {},
    )
    try:
        with urllib.request.urlopen(request) as response:
            return response.status, response.headers, response.read()
    except urllib.error.HTTPError as error:
        # A `304` reaches the caller as an error object; its headers are the
        # ones under test, so it is an observation rather than a failure.
        return error.status, error.headers, error.read()


def assert_content_derived_validators_keep_shards_distinct(
    binary, collision_fixture, root
):
    """Issue 0078, black-box against a real supported client.

    `huggingface_hub` names every blob in its cache after the validator the
    server advertised, so a validator two different files can share makes the
    client store one file's bytes under both paths, report success, and get the
    file count and the byte total right. The fixture revision holds four equally
    sized shards with different bytes, which is what sharded weights look like,
    plus two byte-identical files.

    Both halves of the contract are asserted, because they pull in opposite
    directions:

    * different bytes must never share a validator, and every downloaded file
      must match the digest **the archive recorded**, compared as a digest and
      not as a length;
    * identical bytes are *expected* to share one validator and one blob. That
      is correct, and matches how the Hub deduplicates LFS objects by `oid`. A
      later change must not "fix" the collision by making validators unique per
      path, which would break this.
    """
    fixture = load_module(collision_fixture)
    commit = fixture.COMMIT
    archive = root / "collision-archive"
    cache = root / "collision-cache"
    with server(binary, archive, collision_fixture) as endpoint:
        snapshot_download(
            repo_id=COLLISION_REPO_ID,
            revision="main",
            endpoint=endpoint,
            cache_dir=str(cache),
        )
        recorded = archived_digests(archive, COLLISION_REPO_ID, commit)
        assert sorted(recorded) == sorted(fixture.payloads()), recorded

        snapshot = (
            cache
            / f"models--{COLLISION_REPO_ID.replace('/', '--')}"
            / "snapshots"
            / commit
        )
        # Verified by digest, not by size: a client that stored one shard's
        # bytes under another shard's path has the right file count and the
        # right byte total, which is exactly why this defect was silent.
        corrupt = sorted(
            path
            for path, digest in recorded.items()
            if hashlib.sha256((snapshot / path).read_bytes()).hexdigest() != digest
        )
        assert not corrupt, f"downloaded bytes differ from the archive: {corrupt}"

        blobs = {}
        for path, digest in sorted(recorded.items()):
            blobs[path] = os.path.realpath(snapshot / path)
            # The client named the blob after the advertised validator, which is
            # why the validator has to be the content fingerprint.
            assert Path(blobs[path]).name == digest, (path, blobs[path])

        shards = [
            fixture.shard_name(index) for index in range(1, fixture.SHARD_COUNT + 1)
        ]
        assert len({recorded[shard] for shard in shards}) == len(shards), recorded
        assert len({blobs[shard] for shard in shards}) == len(shards), blobs
        # Equal size, different bytes: the collision this fixture exists for.
        assert len({len(fixture.payloads()[shard]) for shard in shards}) == 1

        # Byte-identical files share one validator and therefore one blob. This
        # is the correct outcome, not a collision to be removed.
        assert blobs["duplicate-one.json"] == blobs["duplicate-two.json"], blobs
        assert recorded["duplicate-one.json"] == recorded["duplicate-two.json"]
        assert len(set(blobs.values())) == len(recorded) - 1, blobs

        # The advertised validator is the recorded digest, on the whole
        # response, on `HEAD`, and on a partial response.
        first, second = shards[0], shards[1]
        expected = f'"{recorded[first]}"'
        status, headers, _ = resolve_headers(
            endpoint, COLLISION_REPO_ID, commit, first, method="HEAD"
        )
        assert status == 200
        assert headers["ETag"] == expected, headers
        assert headers.get("x-linked-etag") is None, headers
        status, headers, body = resolve_headers(
            endpoint, COLLISION_REPO_ID, commit, first, method="GET"
        )
        assert status == 200
        assert headers["ETag"] == expected, headers
        assert hashlib.sha256(body).hexdigest() == recorded[first]
        status, headers, body = resolve_headers(
            endpoint,
            COLLISION_REPO_ID,
            commit,
            first,
            method="GET",
            headers={"Range": "bytes=0-31"},
        )
        assert status == 206
        assert headers["ETag"] == expected, headers
        assert len(body) == 32

        # One file's validator must not validate another file, however equal
        # their lengths.
        status, headers, body = resolve_headers(
            endpoint,
            COLLISION_REPO_ID,
            commit,
            second,
            method="GET",
            headers={"If-None-Match": expected},
        )
        assert status == 200, (status, headers)
        assert headers["ETag"] == f'"{recorded[second]}"', headers
        assert hashlib.sha256(body).hexdigest() == recorded[second]
        status, headers, _ = resolve_headers(
            endpoint,
            COLLISION_REPO_ID,
            commit,
            second,
            method="GET",
            headers={"If-None-Match": f'"{recorded[second]}"'},
        )
        assert status == 304, (status, headers)

    # The archived revision is served from what the manifest already records, so
    # a revision published before this change needs no re-acquisition: upstream
    # is unavailable here (no helper, no HF_ENDPOINT) and the download still
    # reproduces every recorded digest.
    offline_logs = []
    with server(binary, archive, captured_logs=offline_logs) as endpoint:
        offline_cache = root / "collision-offline-cache"
        snapshot_download(
            repo_id=COLLISION_REPO_ID,
            revision=commit,
            endpoint=endpoint,
            cache_dir=str(offline_cache),
        )
        snapshot = (
            offline_cache
            / f"models--{COLLISION_REPO_ID.replace('/', '--')}"
            / "snapshots"
            / commit
        )
        for path, digest in sorted(recorded.items()):
            actual = hashlib.sha256((snapshot / path).read_bytes()).hexdigest()
            assert actual == digest, (path, actual, digest)
    assert '"event":"archive_miss"' not in offline_logs[0], offline_logs[0]
    assert '"event":"acquisition' not in offline_logs[0], offline_logs[0]


def main():
    binary = Path(sys.argv[1])
    helper = Path(sys.argv[2])
    expected_version = sys.argv[3]
    actual_helper = Path(sys.argv[4])
    collision_fixture = Path(sys.argv[5])
    assert huggingface_hub_version == expected_version, (
        f"expected huggingface_hub {expected_version}, got {huggingface_hub_version}"
    )
    with tempfile.TemporaryDirectory() as temporary:
        root = Path(temporary)
        archive = root / "archive"
        acquisition_logs = []
        with localhost_only():
            with server(binary, archive, helper, acquisition_logs) as endpoint:
                for revision, expected_status in [
                    ("missing", 404),
                    ("private", 401),
                    ("unavailable", 502),
                ]:
                    try:
                        HfApi(endpoint=endpoint).repo_info(REPO_ID, revision=revision)
                        raise AssertionError(f"{revision} unexpectedly succeeded")
                    except HfHubHTTPError as error:
                        assert error.response.status_code == expected_status
                info = HfApi(endpoint=endpoint).repo_info(REPO_ID, revision="main")
                assert info.sha == COMMIT
                cold = Path(download(endpoint, root / "cold-client", "main"))
                expected_payloads = {
                    "model.safetensors": b"MODELKEEP-SAFETENSORS-FIXTURE",
                    "model-00001-of-00002.safetensors": b"MODELKEEP-SHARD-ONE",
                    "model-00002-of-00002.safetensors": b"MODELKEEP-SHARD-TWO",
                }
                for relative, expected in expected_payloads.items():
                    assert (cold / relative).read_bytes() == expected
                index = (cold / "model.safetensors.index.json").read_text()
                assert "model-00001-of-00002.safetensors" in index
                assert "model-00002-of-00002.safetensors" in index

                dataset_info = HfApi(endpoint=endpoint).repo_info(
                    REPO_ID, revision="main", repo_type="dataset"
                )
                assert dataset_info.id == REPO_ID
                assert dataset_info.sha == COMMIT
                dataset_payloads = {
                    "README.md": b"# ModelKeep dataset fixture\n",
                    "data/test.csv": b"split,value\ntest,dataset\n",
                    "data/train.csv": b"split,value\ntrain,dataset\n",
                }
                cold_dataset = Path(
                    download(
                        endpoint,
                        root / "cold-dataset-client",
                        "main",
                        repo_type="dataset",
                    )
                )
                for relative, expected in dataset_payloads.items():
                    assert (cold_dataset / relative).read_bytes() == expected

                # The identical repo ID and commit above must identify two distinct
                # archives. Re-read the model after the dataset acquisition to catch
                # an implementation that aliases the durable namespaces.
                assert (cold / "README.md").read_bytes() == b"# ModelKeep model fixture\n"
                model_again = Path(
                    download(endpoint, root / "model-after-dataset", COMMIT)
                )
                assert (model_again / "config.json").read_bytes() == (
                    b'{"model_type":"modelkeep-fixture"}'
                )

                head = urllib.request.Request(
                    f"{endpoint}/{REPO_ID}/resolve/{COMMIT}/config.json",
                    method="HEAD",
                )
                with urllib.request.urlopen(head) as response:
                    assert response.status == 200
                    assert response.headers["Content-Length"] == "34"
                    assert response.read() == b""

                request = urllib.request.Request(
                    f"{endpoint}/{REPO_ID}/resolve/{COMMIT}/config.json",
                    headers={"Range": "bytes=0-4"},
                )
                with urllib.request.urlopen(request) as response:
                    assert response.status == 206
                    assert response.read() == b'{"mod'

                payload_request = urllib.request.Request(
                    f"{endpoint}/{REPO_ID}/resolve/{COMMIT}/model.safetensors",
                    headers={"Range": "bytes=0-8"},
                )
                with urllib.request.urlopen(payload_request) as response:
                    assert response.status == 206
                    assert response.headers["Location"] is None
                    assert response.headers["x-xet-hash"] is None
                    assert response.headers["x-linked-etag"] is None
                    assert response.read() == b"MODELKEEP"

                dataset_head = urllib.request.Request(
                    f"{endpoint}/datasets/{REPO_ID}/resolve/{COMMIT}/data/test.csv",
                    method="HEAD",
                )
                with urllib.request.urlopen(dataset_head) as response:
                    assert response.status == 200
                    assert response.headers["Content-Length"] == str(
                        len(dataset_payloads["data/test.csv"])
                    )
                    assert response.headers["Location"] is None
                    assert response.read() == b""

                dataset_range = urllib.request.Request(
                    f"{endpoint}/datasets/{REPO_ID}/resolve/{COMMIT}/data/test.csv",
                    headers={"Range": "bytes=0-4"},
                )
                with urllib.request.urlopen(dataset_range) as response:
                    assert response.status == 206
                    assert response.headers["Location"] is None
                    assert response.read() == b"split"

            assert len(acquisition_logs) == 1
            assert '"request_kind":"head_file"' in acquisition_logs[0]
            assert '"path":"model.safetensors"' in acquisition_logs[0]

            # Exercise the production helper itself, rather than only the synthetic
            # fixture that independently implements its stdout event contract. The
            # populated first archive acts as a local upstream, so this remains
            # deterministic and does not require internet access.
            actual_archive = root / "actual-helper-archive"
            with server(binary, archive) as upstream_endpoint:
                with server(
                    binary,
                    actual_archive,
                    actual_helper,
                    upstream_endpoint=upstream_endpoint,
                ) as endpoint:
                    actual = Path(
                        download(endpoint, root / "actual-helper-client", "main")
                    )
                    for relative, expected in expected_payloads.items():
                        assert (actual / relative).read_bytes() == expected

            assert_metadata_reports_hub_fields_and_modelkeeps_digest(
                binary, archive, root
            )
            assert_content_derived_validators_keep_shards_distinct(
                binary, collision_fixture, root
            )
            assert_cold_miss_deadline_is_retried_by_the_client(binary, helper, root)
            assert_selected_subset_is_acquired_and_served(
                binary, root, actual_helper, archive
            )
            assert_a_client_filter_narrows_the_first_acquisition(
                binary, root, actual_helper, archive
            )

            # Released archives can outlive the writer that created their manifest.
            # Simulate an old manifest that accidentally listed transient downloader
            # metadata and prove a real supported client never observes or requests it.
            manifest_path = (
                archive
                / "models"
                / "org"
                / "model"
                / "revisions"
                / COMMIT
                / ".modelkeep-manifest.json"
            )
            manifest = json.loads(manifest_path.read_text())
            manifest["files"].extend(
                [
                    {
                        "path": ".modelkeep-staging-lease",
                        "size": 1,
                        "sha256": "0",
                    },
                    {
                        "path": ".cache/huggingface/download.json",
                        "size": 1,
                        "sha256": "0",
                    },
                ]
            )
            manifest_path.write_text(json.dumps(manifest, separators=(",", ":")))

            offline_logs = []
            with server(binary, archive, captured_logs=offline_logs) as endpoint:
                offline = Path(download(endpoint, root / "offline-client", COMMIT))
                for relative, expected in expected_payloads.items():
                    assert (offline / relative).read_bytes() == expected
                offline_dataset = Path(
                    download(
                        endpoint,
                        root / "offline-dataset-client",
                        COMMIT,
                        repo_type="dataset",
                    )
                )
                for relative, expected in dataset_payloads.items():
                    assert (offline_dataset / relative).read_bytes() == expected
                with ThreadPoolExecutor(max_workers=4) as pool:
                    results = list(
                        pool.map(
                            lambda index: download(
                                endpoint, root / f"concurrent-{index}", "main"
                            ),
                            range(4),
                        )
                    )
                assert len(results) == 4
            assert ".modelkeep-staging-lease" not in offline_logs[0]
            assert ".cache/huggingface" not in offline_logs[0]


if __name__ == "__main__":
    main()
