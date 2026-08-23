#!/usr/bin/env python3
import importlib.util
import json
import tempfile
import unittest
from pathlib import Path


SCRIPT = Path(__file__).with_name("qnap_client_acceptance.py")
SPEC = importlib.util.spec_from_file_location("qnap_client_acceptance", SCRIPT)
acceptance = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(acceptance)


class QnapClientAcceptanceTests(unittest.TestCase):
    def test_download_manifest_hashes_model_files_and_ignores_hf_metadata(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            (root / "weights").mkdir()
            (root / "weights/model.bin").write_bytes(b"weights")
            (root / ".cache/huggingface/download").mkdir(parents=True)
            (root / ".cache/huggingface/download/model.metadata").write_text("mutable")

            manifest = acceptance.download_manifest(root)

            self.assertEqual(list(manifest), ["weights/model.bin"])
            self.assertEqual(manifest["weights/model.bin"]["size"], 7)
            self.assertEqual(
                manifest["weights/model.bin"]["sha256"],
                "9a129038d9a00aed0cf6a7ea059ca50a813449061ab87848cf1a13eafdf33b2c",
            )

    def test_file_url_quotes_each_path_component(self):
        record = {
            "configuration": {
                "endpoint": "https://modelkeep.example.ts.net",
                "repo_id": "org/model",
                "revision": "a" * 40,
            }
        }
        self.assertEqual(
            acceptance.file_url(record, "dir/a file?#.bin"),
            "https://modelkeep.example.ts.net/org/model/resolve/"
            + "a" * 40
            + "/dir/a%20file%3F%23.bin",
        )

    def test_finish_requires_every_hardware_phase(self):
        record = {
            "schema_version": 1,
            "created_at": "2026-08-24T00:00:00+00:00",
            "configuration": {
                "endpoint": "https://modelkeep.example.ts.net",
                "admin_endpoint": "https://modelkeep-admin.example.ts.net",
                "qnap_lan_address": "192.0.2.1",
                "repo_id": "org/model",
                "revision": "a" * 40,
            },
            "site": {},
            "client": {},
            "phases": {"preflight": {"status": "passed"}},
        }
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "record.json"
            path.write_text(json.dumps(record))
            args = type("Args", (), {"record": str(path), "output": None})()
            with self.assertRaisesRegex(acceptance.AcceptanceError, "cold"):
                acceptance.finish(args)

    def test_record_write_is_readable_and_validated(self):
        record = {
            "schema_version": 1,
            "configuration": {"repo_id": "org/model", "revision": "b" * 40},
        }
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "nested/record.json"
            acceptance.write_record(path, record)
            self.assertEqual(acceptance.read_record(path), record)

    def test_endpoint_rejects_embedded_credentials(self):
        with self.assertRaises(acceptance.AcceptanceError):
            acceptance.normalize_endpoint("https://user:secret@example.test", "endpoint")


if __name__ == "__main__":
    unittest.main()
