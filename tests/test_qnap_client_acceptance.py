#!/usr/bin/env python3
import importlib.util
import json
import os
import tempfile
import unittest
from pathlib import Path


SCRIPT = Path(__file__).with_name("qnap_client_acceptance.py")
CONFIG_TEMPLATE = Path(
    os.environ.get(
        "MODELKEEP_QNAP_CONFIG_TEMPLATE",
        Path(__file__).parent.parent
        / "docs/deployment/qnap-acceptance-config.example.json",
    )
)
SPEC = importlib.util.spec_from_file_location("qnap_client_acceptance", SCRIPT)
acceptance = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(acceptance)


class QnapClientAcceptanceTests(unittest.TestCase):
    def test_config_template_pins_the_default_public_model(self):
        config = json.loads(CONFIG_TEMPLATE.read_text())
        self.assertEqual(config["repo_id"], "sshleifer/tiny-gpt2")
        self.assertRegex(config["revision"], acceptance.COMMIT_PATTERN)
        self.assertEqual(
            config["revision"], "5f91d94bd9cd7190a9f3216ff93cd1dd95f2c7be"
        )

    def complete_record(self):
        return {
            "schema_version": 1,
            "created_at": "2026-08-24T00:00:00+00:00",
            "configuration": {
                "endpoint": "https://modelkeep.example.ts.net",
                "admin_endpoint": "https://modelkeep-admin.example.ts.net",
                "qnap_lan_address": "192.0.2.1",
                "repo_id": "org/model",
                "revision": "a" * 40,
                "request_timeout_seconds": 10,
                "download_timeout_seconds": 7200,
            },
            "site": {
                "operator": "operator",
                "qnap_model": "QNAP",
                "qts_version": "QuTS hero",
                "container_station_version": "Container Station",
                "archive_share_and_acl": "share and ACL",
                "snapshot_mechanism_and_retention": "snapshot policy",
                "external_backup_target": "backup target",
                "image_tag": "registry.example/modelkeep:v1",
                "image_digest": "sha256:" + "b" * 64,
            },
            "client": {"hostname": "client", "platform": "Linux"},
            "phases": {},
        }

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

    def test_finish_requires_every_release_acceptance_phase(self):
        record = self.complete_record()
        record["phases"] = {"preflight": {"status": "passed"}}
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "record.json"
            path.write_text(json.dumps(record))
            args = type("Args", (), {"record": str(path), "output": None})()
            with self.assertRaisesRegex(acceptance.AcceptanceError, "cold"):
                acceptance.finish(args)

    def test_finish_does_not_require_optional_restore_drill(self):
        record = self.complete_record()
        record["phases"] = {
            name: {
                "status": "passed",
                "finished_at": "2026-09-23T00:00:00+00:00",
            }
            for name in acceptance.REQUIRED_PHASES
        }
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "record.json"
            output = Path(temporary) / "record.md"
            path.write_text(json.dumps(record))
            args = type(
                "Args", (), {"record": str(path), "output": str(output)}
            )()
            acceptance.finish(args)
            summary = output.read_text()
            completed = json.loads(path.read_text())

        self.assertIn("completed_at", completed)
        self.assertIn("## Optional disaster-recovery drills", summary)
        self.assertIn("| post-restore | not run |", summary)

    def test_summary_records_completed_optional_restore_drill(self):
        record = self.complete_record()
        record["phases"]["post-restore"] = {
            "status": "passed",
            "finished_at": "2026-09-23T01:00:00+00:00",
        }

        summary = acceptance.render_summary(record)

        self.assertIn(
            "| post-restore | passed | 2026-09-23T01:00:00+00:00 |", summary
        )

    def test_record_write_is_readable_and_validated(self):
        record = self.complete_record()
        record["configuration"]["revision"] = "b" * 40
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "nested/record.json"
            acceptance.write_record(path, record)
            self.assertEqual(acceptance.read_record(path), record)

    def test_endpoint_rejects_embedded_credentials(self):
        with self.assertRaises(acceptance.AcceptanceError):
            acceptance.normalize_endpoint("https://user:secret@example.test", "endpoint")

    def test_record_rejects_identical_download_and_admin_endpoints(self):
        record = self.complete_record()
        record["configuration"]["admin_endpoint"] = record["configuration"]["endpoint"]
        with self.assertRaisesRegex(acceptance.AcceptanceError, "must be distinct"):
            acceptance.validate_record(record)

    def test_initialize_reads_site_values_from_config_file(self):
        initial = {
            "endpoint": "https://modelkeep.example.ts.net",
            "admin_endpoint": "https://modelkeep-admin.example.ts.net",
            "qnap_lan_address": "192.0.2.1",
            "repo_id": "org/model",
            "revision": "A" * 40,
            "operator": "operator",
            "qnap_model": "model",
            "qts_version": "qts",
            "container_station_version": "container-station",
            "archive_share_and_acl": "share and acl",
            "snapshot_mechanism_and_retention": "snapshots",
            "external_backup_target": "backup",
            "image_tag": "registry.example/modelkeep:v1",
            "image_digest": "sha256:" + "c" * 64,
        }
        with tempfile.TemporaryDirectory() as temporary:
            config = Path(temporary) / "site.json"
            record = Path(temporary) / "record.json"
            config.write_text(json.dumps(initial))
            args = type(
                "Args",
                (),
                {"record": str(record), "config": str(config), "force": False},
            )()
            acceptance.initialize(args)
            loaded = acceptance.read_record(record)

        self.assertEqual(loaded["configuration"]["endpoint"], initial["endpoint"])
        self.assertEqual(loaded["configuration"]["revision"], "a" * 40)
        self.assertEqual(loaded["site"]["image_digest"], initial["image_digest"])


if __name__ == "__main__":
    unittest.main()
