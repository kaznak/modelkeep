#!/usr/bin/env python3
import contextlib
import os
import socket
import subprocess
import sys
import tempfile
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
def server(binary, archive, helper=None, captured_logs=None):
    port = unused_port()
    endpoint = f"http://127.0.0.1:{port}"
    environment = os.environ.copy()
    if helper:
        environment["MODELKEEP_HF_PYTHON"] = sys.executable
        environment["MODELKEEP_HF_HELPER"] = str(helper)
    else:
        environment.pop("MODELKEEP_HF_PYTHON", None)
        environment.pop("MODELKEEP_HF_HELPER", None)
    process = subprocess.Popen(
        [str(binary), "serve", str(archive), f"127.0.0.1:{port}"],
        env=environment,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.PIPE,
        text=True,
    )
    try:
        for _ in range(100):
            if process.poll() is not None:
                raise RuntimeError(process.stderr.read())
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
        if captured_logs is not None:
            captured_logs.append(process.stderr.read())


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


def download(endpoint, destination, revision):
    return snapshot_download(
        repo_id=REPO_ID,
        revision=revision,
        endpoint=endpoint,
        local_dir=str(destination),
    )


def main():
    binary = Path(sys.argv[1])
    helper = Path(sys.argv[2])
    expected_version = sys.argv[3]
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

            assert len(acquisition_logs) == 1
            assert '"request_kind":"head_file"' in acquisition_logs[0]
            assert '"path":"model.safetensors"' in acquisition_logs[0]

            with server(binary, archive) as endpoint:
                offline = Path(download(endpoint, root / "offline-client", COMMIT))
                for relative, expected in expected_payloads.items():
                    assert (offline / relative).read_bytes() == expected
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


if __name__ == "__main__":
    main()
