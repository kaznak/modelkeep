#!/usr/bin/env python3
import contextlib
import json
import os
import socket
import subprocess
import sys
import tempfile
import threading
import time
import urllib.request
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path

from huggingface_hub import HfApi, snapshot_download
from huggingface_hub import __version__ as huggingface_hub_version
from huggingface_hub.errors import HfHubHTTPError


COMMIT = "a" * 40
REPO_ID = "org/model"


def unused_port():
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        return listener.getsockname()[1]


@contextlib.contextmanager
def server(binary, archive, helper=None, captured_logs=None, upstream_endpoint=None):
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


def main():
    binary = Path(sys.argv[1])
    helper = Path(sys.argv[2])
    expected_version = sys.argv[3]
    actual_helper = Path(sys.argv[4])
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
