#!/usr/bin/env python3
import shutil, socket, subprocess, sys, tempfile, time, urllib.request
from pathlib import Path

COMMIT = "a" * 40

def free_port():
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0)); return sock.getsockname()[1]

def main():
    binary = Path(sys.argv[1])
    with tempfile.TemporaryDirectory() as temporary:
        root = Path(temporary); cache = root / "cache" / "models--org--model"
        blob = cache / "blobs/config"; snapshot = cache / "snapshots" / COMMIT
        blob.parent.mkdir(parents=True); snapshot.mkdir(parents=True)
        blob.write_bytes(b"restored"); (snapshot / "config.json").symlink_to(blob)
        (cache / "refs").mkdir(); (cache / "refs/main").write_text(COMMIT)
        archive = root / "archive"
        subprocess.run([binary, "import-hf-cache", root / "cache", archive], check=True)
        subprocess.run([binary, "verify", archive, "org/model", COMMIT], check=True)
        backup = root / "backup"; restored = root / "restored"
        shutil.copytree(archive, backup); shutil.copytree(backup, restored)
        subprocess.run([binary, "verify", restored, "org/model", COMMIT], check=True)
        port = free_port(); process = subprocess.Popen([binary, "serve", restored, f"127.0.0.1:{port}"])
        try:
            url = f"http://127.0.0.1:{port}/org/model/resolve/{COMMIT}/config.json"
            for _ in range(100):
                try:
                    with urllib.request.urlopen(url, timeout=.2) as response:
                        assert response.read() == b"restored"; break
                except OSError: time.sleep(.05)
            else: raise AssertionError("restored archive was not served")
        finally:
            process.terminate(); process.wait(timeout=5)

if __name__ == "__main__": main()
