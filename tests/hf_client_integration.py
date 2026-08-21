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
from huggingface_hub.errors import HfHubHTTPError


COMMIT = "a" * 40
REPO_ID = "org/model"


def unused_port():
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        return listener.getsockname()[1]


@contextlib.contextmanager
def server(binary, archive, helper=None):
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
    with tempfile.TemporaryDirectory() as temporary:
        root = Path(temporary)
        archive = root / "archive"
        with server(binary, archive, helper) as endpoint:
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
            download(endpoint, root / "cold-client", "main")

            request = urllib.request.Request(
                f"{endpoint}/{REPO_ID}/resolve/{COMMIT}/config.json",
                headers={"Range": "bytes=0-4"},
            )
            with urllib.request.urlopen(request) as response:
                assert response.status == 206
                assert response.read() == b'{"mod'

        with server(binary, archive) as endpoint, localhost_only():
            download(endpoint, root / "offline-client", COMMIT)
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
