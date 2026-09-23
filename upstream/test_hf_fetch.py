import contextlib
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
    def test_resumed_client_stdout_cannot_corrupt_helper_event_channel(self):
        api = MovingRefApi()
        protocol = io.StringIO()
        diagnostics = io.StringIO()

        def noisy_download(**kwargs):
            partial = Path(
                kwargs["local_dir"],
                ".cache",
                "huggingface",
                "download",
                "model.incomplete",
            )
            self.assertEqual(partial.read_bytes(), b"retained-partial")
            print('{"resumed":true,"transport":"diagnostic"}')
            Path(kwargs["local_dir"], "config.json").write_bytes(b"12345678")
            Path(kwargs["local_dir"], "model.bin").write_bytes(b"0123456789")

        with tempfile.TemporaryDirectory() as output:
            partial = Path(
                output, ".cache", "huggingface", "download", "model.incomplete"
            )
            partial.parent.mkdir(parents=True)
            partial.write_bytes(b"retained-partial")
            with contextlib.redirect_stdout(protocol), contextlib.redirect_stderr(diagnostics):
                result = hf_fetch.acquire(
                    "org/model",
                    COMMIT_A,
                    output,
                    api=api,
                    download=noisy_download,
                    progress=hf_fetch.ProgressReporter(stream=protocol),
                )

        events = [json.loads(line) for line in protocol.getvalue().splitlines()]
        self.assertTrue(events)
        self.assertTrue(
            all(event.get("type") in ("progress", "resolved") for event in events)
        )
        self.assertNotIn("transport", protocol.getvalue())
        self.assertIn('"transport":"diagnostic"', diagnostics.getvalue())
        self.assertEqual(
            result, {"commit": COMMIT_A, "files": ["config.json", "model.bin"]}
        )

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

    def test_progress_includes_retained_incomplete_file_bytes(self):
        stream = io.StringIO()
        reporter = hf_fetch.ProgressReporter(stream=stream, minimum_interval=0)
        with tempfile.TemporaryDirectory() as output:
            reporter.set_expected(output, [("a.bin", 4), ("b.bin", 6)])
            Path(output, "a.bin").write_bytes(b"aaaa")
            partial = Path(output, ".cache", "huggingface", "download", "b.incomplete")
            partial.parent.mkdir(parents=True)
            partial.write_bytes(b"bbb")
            reporter._report_files()
            partial.write_bytes(b"bbbbbb")
            reporter._report_files()
            Path(output, "b.bin").write_bytes(b"bbbbbb")
            partial.unlink()
            reporter._report_files(force=True, finalized=True)

        events = [json.loads(line) for line in stream.getvalue().splitlines()]
        byte_events = [event for event in events if event.get("unit") == "bytes"]
        file_events = [event for event in events if event.get("unit") == "files"]
        self.assertEqual(byte_events[-1], {
            "type": "progress", "version": 1, "phase": "downloading", "unit": "bytes",
            "completed": 10, "total": 10,
        })
        self.assertIn(7, [event["completed"] for event in byte_events])
        self.assertEqual(file_events[-1]["completed"], 2)
        self.assertEqual(file_events[-1]["total"], 2)

    def test_unchanged_counters_do_not_emit_false_progress_heartbeats(self):
        stream = io.StringIO()
        reporter = hf_fetch.ProgressReporter(stream=stream, minimum_interval=0)
        with tempfile.TemporaryDirectory() as output:
            reporter.set_expected(output, [("model.bin", 10)])
            reporter._report_files()
            reporter._report_files()

        byte_events = [
            event
            for event in map(json.loads, stream.getvalue().splitlines())
            if event.get("unit") == "bytes"
        ]
        self.assertEqual(len(byte_events), 1)

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
        self.assertEqual(download_calls[0]["repo_type"], "model")
        self.assertEqual(result, {"commit": COMMIT_A, "files": ["config.json", "model.bin"]})

    def test_dataset_type_is_used_for_metadata_and_download(self):
        api = MovingRefApi()
        download_calls = []

        def download(**kwargs):
            download_calls.append(kwargs)
            Path(kwargs["local_dir"], "data.jsonl").write_bytes(b'{"value":1}\n')

        with tempfile.TemporaryDirectory() as output:
            api.repo_info = lambda repo_id, revision, repo_type, files_metadata=False: (
                setattr(api, "request", (repo_id, revision, repo_type, files_metadata))
                or SimpleNamespace(
                    sha=COMMIT_A,
                    siblings=[SimpleNamespace(rfilename="data.jsonl", size=12)],
                )
            )
            result = hf_fetch.acquire(
                "org/shared",
                "main",
                output,
                repo_type="dataset",
                api=api,
                download=download,
            )

        self.assertEqual(api.request, ("org/shared", "main", "dataset", True))
        self.assertEqual(download_calls[0]["repo_type"], "dataset")
        self.assertEqual(result, {"commit": COMMIT_A, "files": ["data.jsonl"]})

    def subset_api(self):
        return SimpleNamespace(
            repo_info=lambda repo_id, revision, repo_type, files_metadata=False: (
                SimpleNamespace(
                    sha=COMMIT_A,
                    siblings=[
                        SimpleNamespace(rfilename="config.json", size=6),
                        SimpleNamespace(rfilename="q4/a.gguf", size=3),
                        SimpleNamespace(rfilename="q4/b.gguf", size=3),
                        SimpleNamespace(rfilename="q8/a.gguf", size=3),
                    ],
                )
            )
        )

    def test_include_and_exclude_patterns_are_passed_to_the_official_client(self):
        api = self.subset_api()
        download_calls = []

        def download(**kwargs):
            download_calls.append(kwargs)
            target = Path(kwargs["local_dir"], "q4", "a.gguf")
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_bytes(b"aaa")

        with tempfile.TemporaryDirectory() as output:
            with contextlib.redirect_stdout(io.StringIO()):
                result = hf_fetch.acquire(
                    "org/model",
                    "main",
                    output,
                    files=["q4/"],
                    exclude=["q4/b.gguf"],
                    api=api,
                    download=download,
                )

        self.assertEqual(download_calls[0]["allow_patterns"], ["q4/"])
        self.assertEqual(download_calls[0]["ignore_patterns"], ["q4/b.gguf"])
        self.assertEqual(result, {"commit": COMMIT_A, "files": ["q4/a.gguf"]})

    def test_whole_repository_acquisition_passes_no_patterns(self):
        api = self.subset_api()
        download_calls = []

        def download(**kwargs):
            download_calls.append(kwargs)
            Path(kwargs["local_dir"], "config.json").write_bytes(b"config")

        with tempfile.TemporaryDirectory() as output:
            with contextlib.redirect_stdout(io.StringIO()):
                result = hf_fetch.acquire(
                    "org/model", "main", output, api=api, download=download
                )

        self.assertIsNone(download_calls[0]["allow_patterns"])
        self.assertIsNone(download_calls[0]["ignore_patterns"])
        self.assertEqual(result["files"], ["config.json"])

    def test_files_outside_the_selection_are_not_recorded(self):
        api = self.subset_api()

        def download(**kwargs):
            for path in ("config.json", "q4/a.gguf", "q4/b.gguf", "q8/a.gguf"):
                target = Path(kwargs["local_dir"], path)
                target.parent.mkdir(parents=True, exist_ok=True)
                target.write_bytes(b"xxx")

        with tempfile.TemporaryDirectory() as output:
            with contextlib.redirect_stdout(io.StringIO()):
                result = hf_fetch.acquire(
                    "org/model",
                    "main",
                    output,
                    files=["q4/"],
                    exclude=["q4/b.gguf"],
                    api=api,
                    download=download,
                )

        self.assertEqual(result["files"], ["q4/a.gguf"])

    def test_expected_files_honour_include_and_exclude(self):
        info = self.subset_api().repo_info("org/model", "main", "model", True)

        self.assertEqual(
            hf_fetch.expected_files(info),
            [
                ("config.json", 6),
                ("q4/a.gguf", 3),
                ("q4/b.gguf", 3),
                ("q8/a.gguf", 3),
            ],
        )
        self.assertEqual(
            hf_fetch.expected_files(info, ["q4/"], ["q4/b.gguf"]),
            [("q4/a.gguf", 3)],
        )
        self.assertEqual(
            hf_fetch.expected_files(info, None, ["q4/*", "q8/*"]),
            [("config.json", 6)],
        )

    def test_directory_pattern_is_expanded_like_the_official_client(self):
        self.assertEqual(hf_fetch.normalized_pattern("q4/"), "q4/*")
        self.assertEqual(hf_fetch.normalized_pattern("q4/*.gguf"), "q4/*.gguf")
        self.assertTrue(hf_fetch.selected("q4/a.gguf", ["q4/"], None))
        self.assertFalse(hf_fetch.selected("q40/a.gguf", ["q4/"], None))
        self.assertFalse(hf_fetch.selected("q4/a.gguf", ["q4/"], ["*.gguf"]))

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
