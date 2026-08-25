import io
import json
import tempfile
import unittest
from pathlib import Path
from types import SimpleNamespace

import hf_fetch


COMMIT_A = "a" * 40


class MovingRefApi:
    def repo_info(self, repo_id, revision, repo_type, files_metadata=False):
        self.request = (repo_id, revision, repo_type, files_metadata)
        return SimpleNamespace(
            sha=COMMIT_A,
            siblings=[
                SimpleNamespace(rfilename="config.json", size=8),
                SimpleNamespace(rfilename="model.bin", size=10),
            ],
        )


class HfFetchTests(unittest.TestCase):
    def test_progress_reporter_emits_machine_readable_bounded_progress(self):
        stream = io.StringIO()
        reporter = hf_fetch.ProgressReporter(stream=stream, minimum_interval=0)
        with tempfile.TemporaryDirectory() as output:
            reporter.set_expected(output, [("model.bin", 10)])
            progress_type = reporter.tqdm_class()
            progress = progress_type(total=10, unit="B")
            progress.update(4)
            Path(output, "model.bin").write_bytes(b"0123456789")
            progress.close()

        events = [json.loads(line) for line in stream.getvalue().splitlines()]
        self.assertTrue(events)
        self.assertTrue(all(event["type"] == "progress" for event in events))
        byte_events = [event for event in events if event.get("unit") == "bytes"]
        self.assertEqual(byte_events[-1]["completed"], 10)
        self.assertEqual(byte_events[-1]["total"], 10)

    def test_progress_counts_only_complete_materialized_files(self):
        stream = io.StringIO()
        reporter = hf_fetch.ProgressReporter(stream=stream, minimum_interval=0)
        with tempfile.TemporaryDirectory() as output:
            reporter.set_expected(output, [("a.bin", 4), ("b.bin", 6)])
            progress_type = reporter.tqdm_class()
            first = progress_type(total=4, unit="B")
            second = progress_type(total=6, unit="B")
            first.update(4)
            Path(output, "a.bin").write_bytes(b"aaaa")
            first.close()
            second.update(3)
            Path(output, "b.bin").write_bytes(b"bbbbbb")
            second.close()

        events = [json.loads(line) for line in stream.getvalue().splitlines()]
        byte_events = [event for event in events if event.get("unit") == "bytes"]
        file_events = [event for event in events if event.get("unit") == "files"]
        self.assertEqual(byte_events[-1], {
            "type": "progress", "version": 1, "phase": "downloading", "unit": "bytes",
            "completed": 10, "total": 10,
        })
        self.assertIn(4, [event["completed"] for event in byte_events])
        self.assertEqual(file_events[-1]["completed"], 2)
        self.assertEqual(file_events[-1]["total"], 2)

    def test_unknown_file_sizes_do_not_manufacture_a_byte_total(self):
        stream = io.StringIO()
        reporter = hf_fetch.ProgressReporter(stream=stream, minimum_interval=0)
        with tempfile.TemporaryDirectory() as output:
            reporter.set_expected(output, [("model.bin", None)])
            Path(output, "model.bin").write_bytes(b"payload")
            reporter._report_files()
            reporter._report_files(force=True, finalized=True)

        events = [json.loads(line) for line in stream.getvalue().splitlines()]
        byte_events = [event for event in events if event.get("unit") == "bytes"]
        file_events = [event for event in events if event.get("unit") == "files"]
        self.assertTrue(all("total" not in event for event in byte_events))
        self.assertEqual(byte_events[-1]["completed"], 7)
        self.assertEqual(file_events[-1]["completed"], 1)

    def test_download_is_pinned_to_commit_resolved_before_ref_moves(self):
        api = MovingRefApi()
        download_calls = []

        def download(**kwargs):
            download_calls.append(kwargs)
            self.assertEqual(kwargs["revision"], COMMIT_A)
            Path(kwargs["local_dir"], "config.json").write_bytes(b"commit-a")
            Path(kwargs["local_dir"], "model.bin").write_bytes(b"0123456789")
            metadata = Path(kwargs["local_dir"], ".cache", "huggingface")
            metadata.mkdir(parents=True)
            Path(metadata, "download.json").write_bytes(b"helper metadata")

        with tempfile.TemporaryDirectory() as output:
            result = hf_fetch.acquire(
                "org/model", "main", output, api=api, download=download
            )

        self.assertEqual(api.request, ("org/model", "main", "model", True))
        self.assertEqual(len(download_calls), 1)
        self.assertEqual(result, {"commit": COMMIT_A, "files": ["config.json", "model.bin"]})

    def test_malformed_resolved_commit_is_rejected_before_download(self):
        api = MovingRefApi()
        api.repo_info = lambda *args, **kwargs: SimpleNamespace(sha="not-a-commit")
        download_called = False

        def download(**kwargs):
            nonlocal download_called
            download_called = True

        with tempfile.TemporaryDirectory() as output:
            with self.assertRaisesRegex(ValueError, "malformed commit identity"):
                hf_fetch.acquire(
                    "org/model", "main", output, api=api, download=download
                )

        self.assertFalse(download_called)


if __name__ == "__main__":
    unittest.main()
