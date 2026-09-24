import contextlib
import hashlib
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
            result,
            {
                "commit": COMMIT_A,
                "files": ["config.json", "model.bin"],
                "repository_files": [{"path": "config.json", "size": 8}, {"path": "model.bin", "size": 10}],
            },
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
        self.assertEqual(
            result,
            {
                "commit": COMMIT_A,
                "files": ["config.json", "model.bin"],
                "repository_files": [{"path": "config.json", "size": 8}, {"path": "model.bin", "size": 10}],
            },
        )

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
        self.assertEqual(
            result,
            {
                "commit": COMMIT_A,
                "files": ["data.jsonl"],
                "repository_files": [{"path": "data.jsonl", "size": 12}],
            },
        )

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
        self.assertEqual(
            result,
            {
                "commit": COMMIT_A,
                "files": ["q4/a.gguf"],
                # Recorded whole, not narrowed by the selection: the file list is
                # a property of the commit, which is immutable.
                "repository_files": [
                    {"path": "config.json", "size": 6},
                    {"path": "q4/a.gguf", "size": 3},
                    {"path": "q4/b.gguf", "size": 3},
                    {"path": "q8/a.gguf", "size": 3},
                ],
            },
        )

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

    def test_resolve_only_inventory_lists_the_selection_without_downloading(self):
        api = self.subset_api()
        protocol = io.StringIO()

        with contextlib.redirect_stdout(protocol):
            result = hf_fetch.inventory(
                "org/model", "main", files=["q4/"], api=api
            )

        self.assertEqual(result["commit"], COMMIT_A)
        self.assertEqual(result["files"], ["q4/a.gguf", "q4/b.gguf"])
        self.assertEqual(result["sizes"], {"q4/a.gguf": 3, "q4/b.gguf": 3})
        events = [json.loads(line) for line in protocol.getvalue().splitlines()]
        self.assertEqual([event["type"] for event in events], ["resolved"])
        self.assertEqual(events[0]["commit"], COMMIT_A)

    def test_resolve_only_inventory_may_legitimately_match_nothing(self):
        with contextlib.redirect_stdout(io.StringIO()):
            result = hf_fetch.inventory(
                "org/model", "main", files=["absent/*"], api=self.subset_api()
            )

        self.assertEqual(result["files"], [])
        self.assertEqual(result["sizes"], {})
        self.assertEqual(result["commit"], COMMIT_A)

    def test_per_file_upstream_metadata_is_recorded_for_the_whole_commit(self):
        """Issue 0074/0078: one `repo_info` call already carries both validators.

        `blob_id` is what upstream serves as the `ETag` of a non-LFS file and
        `lfs.sha256` what it serves as `x-linked-etag`, so both are recorded as
        upstream reported them. A field upstream did not report is omitted, never
        guessed.
        """
        info = SimpleNamespace(
            siblings=[
                SimpleNamespace(
                    rfilename="model.safetensors",
                    size=520212,
                    blob_id="b" * 40,
                    lfs=SimpleNamespace(sha256="c" * 64, size=520212),
                ),
                SimpleNamespace(rfilename="config.json", size=6, blob_id="d" * 40),
                SimpleNamespace(rfilename="unknown.bin", size=None, blob_id=None),
            ]
        )

        self.assertEqual(
            hf_fetch.repository_file_metadata(info),
            [
                {"path": "config.json", "size": 6, "blob_id": "d" * 40},
                {
                    "path": "model.safetensors",
                    "size": 520212,
                    "blob_id": "b" * 40,
                    "lfs_sha256": "c" * 64,
                },
                {"path": "unknown.bin"},
            ],
        )

    def test_recorded_file_list_is_not_narrowed_by_the_selection(self):
        api = self.subset_api()

        with contextlib.redirect_stdout(io.StringIO()):
            result = hf_fetch.inventory(
                "org/model", "main", files=["q4/"], api=api
            )

        self.assertEqual(result["files"], ["q4/a.gguf", "q4/b.gguf"])
        self.assertEqual(
            [entry["path"] for entry in result["repository_files"]],
            ["config.json", "q4/a.gguf", "q4/b.gguf", "q8/a.gguf"],
        )

    def test_internal_archive_paths_never_enter_the_recorded_file_list(self):
        info = SimpleNamespace(
            siblings=[
                SimpleNamespace(rfilename=".modelkeep-manifest.json", size=1),
                SimpleNamespace(rfilename=".cache/huggingface/x", size=1),
                SimpleNamespace(rfilename="config.json", size=6),
            ]
        )

        self.assertEqual(
            hf_fetch.repository_file_metadata(info),
            [{"path": "config.json", "size": 6}],
        )

    def test_internal_archive_paths_never_enter_the_expected_set(self):
        info = SimpleNamespace(
            siblings=[
                SimpleNamespace(rfilename=".modelkeep-manifest.json", size=1),
                SimpleNamespace(rfilename=".cache/huggingface/x", size=1),
                SimpleNamespace(rfilename="config.json", size=6),
            ]
        )

        self.assertEqual(hf_fetch.expected_files(info), [("config.json", 6)])

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


class HttpFailure(hf_fetch.HfHubHTTPError):
    """An HTTP failure carrying the status the client attached.

    `__init__` is replaced rather than delegated because the two supported
    client versions declare different constructor signatures for it; the
    helper's classification depends only on the exception type and on
    `response.status_code`, which is what this reproduces.
    """

    def __init__(self, message, status):
        Exception.__init__(self, message)
        self.response = SimpleNamespace(status_code=status)


class GatedFailure(hf_fetch.GatedRepoError):
    def __init__(self, message):
        Exception.__init__(self, message)
        self.response = SimpleNamespace(status_code=403)


class MissingRepositoryFailure(hf_fetch.RepositoryNotFoundError):
    def __init__(self, message):
        Exception.__init__(self, message)
        self.response = None


class MissingRevisionFailure(hf_fetch.RevisionNotFoundError):
    def __init__(self, message):
        Exception.__init__(self, message)
        self.response = None


class FailureReportingTests(unittest.TestCase):
    """Issue 0084: a failed acquisition has to say why, without saying a secret."""

    TOKEN = "hf_" + "A1b2C3d4E5f6G7h8I9j0"
    SIGNATURE = "f" * 64
    SIGNED_URL = (
        "https://cas-bridge.xethub.hf.co/xet-bridge-us/deadbeef/0123456789abcdef"
        "?X-Amz-Signature=" + SIGNATURE + "&X-Amz-Credential=AKIAEXAMPLEKEY%2Fus-east-1"
    )

    def test_token_and_signed_url_shaped_strings_never_reach_the_event(self):
        jwt = "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiJtb2RlbGtlZXAifQ.c2lnbmF0dXJl"
        error = RuntimeError(
            "xet_get failed for "
            + self.SIGNED_URL
            + " with Authorization: Bearer "
            + self.TOKEN
            + " and x-xet-access-token: "
            + jwt
            + " (token="
            + self.TOKEN
            + ")"
        )
        serialized = json.dumps(hf_fetch.failure_event(error))
        for secret in (
            self.TOKEN,
            self.SIGNATURE,
            self.SIGNED_URL,
            jwt,
            "X-Amz-Signature",
            "AKIAEXAMPLEKEY",
        ):
            self.assertNotIn(secret, serialized)
        # The endpoint identity survives, because telling a hub failure from a
        # CAS failure is the point of keeping a reason at all.
        self.assertIn("https://cas-bridge.xethub.hf.co/<redacted>", serialized)
        self.assertIn(hf_fetch.REDACTED_TOKEN, serialized)

    def test_a_url_is_reduced_to_its_endpoint_without_userinfo(self):
        self.assertEqual(
            hf_fetch.sanitized_text("see https://user:secret@host.example:8443/a/b?c=d end"),
            "see https://host.example:8443/<redacted> end",
        )
        self.assertNotIn(
            "secret", hf_fetch.sanitized_text("https://user:secret@host.example/x")
        )
        self.assertEqual(hf_fetch.sanitized_text("scheme://"), hf_fetch.REDACTED_URL)

    def test_permission_denied_in_the_transfer_is_a_client_failure_with_its_reason(self):
        """The Issue 0086 shape: the client could not write its own cache.

        This is the failure that used to be recorded as `failed` with nothing
        else. It now names the class, the exception and what the client said.
        """
        api = MovingRefApi()

        def unwritable_cache(**kwargs):
            raise PermissionError(13, "Permission denied", "/hf-home/hub")

        with tempfile.TemporaryDirectory() as output:
            with self.assertRaises(PermissionError) as raised:
                with contextlib.redirect_stdout(io.StringIO()):
                    hf_fetch.acquire(
                        "org/model",
                        COMMIT_A,
                        output,
                        api=api,
                        download=unwritable_cache,
                    )

        event = hf_fetch.failure_event(raised.exception)
        self.assertEqual(event["class"], hf_fetch.CLASS_CLIENT_FAILURE)
        self.assertEqual(event["exception"], "PermissionError")
        self.assertIn("Permission denied", event["message"])
        self.assertIn("/hf-home/hub", event["message"])
        self.assertEqual(
            hf_fetch.EXIT_CODES[event["class"]],
            14,
        )

    def test_every_class_is_reported_with_its_own_exit_code(self):
        cases = [
            (GatedFailure("gated"), hf_fetch.CLASS_UNAUTHORIZED, 12),
            (MissingRepositoryFailure("no repo"), hf_fetch.CLASS_NOT_FOUND, 11),
            (hf_fetch.EntryNotFoundError("no file"), hf_fetch.CLASS_NOT_FOUND, 11),
            (MissingRevisionFailure("no revision"), hf_fetch.CLASS_NOT_FOUND, 11),
            (HttpFailure("not found", 404), hf_fetch.CLASS_NOT_FOUND, 11),
            (HttpFailure("unauthorized", 401), hf_fetch.CLASS_UNAUTHORIZED, 12),
            (HttpFailure("forbidden", 403), hf_fetch.CLASS_UNAUTHORIZED, 12),
            (HttpFailure("too many requests", 429), hf_fetch.CLASS_RATE_LIMITED, 13),
            (HttpFailure("bad gateway", 502), hf_fetch.CLASS_UNAVAILABLE, 10),
            (HttpFailure("teapot", 418), hf_fetch.CLASS_CLIENT_FAILURE, 14),
            (ConnectionError("refused"), hf_fetch.CLASS_UNAVAILABLE, 10),
            (TimeoutError("timed out"), hf_fetch.CLASS_UNAVAILABLE, 10),
            (ValueError("unsafe upstream path"), hf_fetch.CLASS_CLIENT_FAILURE, 14),
        ]
        for error, expected_class, expected_code in cases:
            with self.subTest(error=type(error).__name__, status=str(error)):
                stream = io.StringIO()
                code = hf_fetch.report_failure(error, stream=stream)
                lines = stream.getvalue().splitlines()
                self.assertEqual(len(lines), 1)
                event = json.loads(lines[0])
                self.assertEqual(event["type"], "failure")
                self.assertEqual(event["version"], 1)
                self.assertEqual(event["class"], expected_class)
                self.assertEqual(event["exception"], type(error).__name__)
                self.assertEqual(code, expected_code)

    def test_a_gated_repository_is_authorization_not_absence(self):
        """`GatedRepoError` subclasses `RepositoryNotFoundError` upstream, so the
        order of the checks is the difference between the two answers."""
        self.assertTrue(
            issubclass(hf_fetch.GatedRepoError, hf_fetch.RepositoryNotFoundError)
        )
        self.assertEqual(
            hf_fetch.failure_class(GatedFailure("gated")),
            hf_fetch.CLASS_UNAUTHORIZED,
        )

    def test_an_unclassifiable_exception_still_reports_its_type_and_message(self):
        class XetRuntimeFailure(Exception):
            pass

        event = hf_fetch.failure_event(XetRuntimeFailure("cas shard write refused"))
        self.assertEqual(event["class"], hf_fetch.CLASS_CLIENT_FAILURE)
        self.assertEqual(event["exception"], "XetRuntimeFailure")
        self.assertEqual(
            event["message"], "XetRuntimeFailure: cas shard write refused"
        )

    def test_the_immediate_cause_is_reported_with_the_failure(self):
        cause = OSError(28, "No space left on device")
        error = RuntimeError("transfer aborted")
        error.__cause__ = cause
        message = hf_fetch.failure_event(error)["message"]
        self.assertIn("RuntimeError: transfer aborted", message)
        self.assertIn("No space left on device", message)

    def test_a_reported_message_is_one_bounded_printable_line(self):
        error = RuntimeError(
            'first\n{"event":"archive_published","repo_id":"org/forged"}\t'
            + "x" * 4096
        )
        event = hf_fetch.failure_event(error)
        self.assertNotIn("\n", event["message"])
        self.assertNotIn("\t", event["message"])
        self.assertTrue(event["message"].endswith(hf_fetch.TRUNCATION_MARKER))
        self.assertEqual(
            len(event["message"]),
            hf_fetch.MESSAGE_LIMIT + len(hf_fetch.TRUNCATION_MARKER),
        )
        self.assertEqual(len(json.dumps(event).splitlines()), 1)

    def test_the_failure_event_goes_to_the_protocol_channel(self):
        protocol = io.StringIO()
        diagnostics = io.StringIO()
        with contextlib.redirect_stdout(protocol), contextlib.redirect_stderr(
            diagnostics
        ):
            code = hf_fetch.report_failure(ConnectionError("refused"))
        self.assertEqual(code, 10)
        self.assertEqual(json.loads(protocol.getvalue())["class"], "unavailable")
        self.assertEqual(diagnostics.getvalue(), "")

    def test_every_class_has_an_exit_code_and_no_code_is_shared(self):
        classes = (
            hf_fetch.CLASS_UNAVAILABLE,
            hf_fetch.CLASS_NOT_FOUND,
            hf_fetch.CLASS_UNAUTHORIZED,
            hf_fetch.CLASS_RATE_LIMITED,
            hf_fetch.CLASS_CLIENT_FAILURE,
        )
        self.assertEqual(sorted(hf_fetch.EXIT_CODES), sorted(classes))
        self.assertEqual(len(set(hf_fetch.EXIT_CODES.values())), len(classes))
        # Exit 1 is reserved for a helper that failed without reporting a class:
        # the parent reports that as the helper's own contract failure.
        self.assertNotIn(1, hf_fetch.EXIT_CODES.values())



# --- In-flight byte accounting (Issue 0082) ---------------------------------
#
# Where the official client stages a file's bytes while it transfers: its
# local-folder layout puts a file's download metadata at
# `<output>/.cache/huggingface/download/<path in repo>.metadata` and the partial
# bytes next to that metadata file, under a hashed basename ending in
# `.incomplete`. A file in a repository subdirectory therefore stages into a
# mirrored subdirectory, which is the part the reporter has to follow.
#
# These tests reconstruct only the shape of that layout, never the client's own
# private path helpers, and they do not stand alone: the mirrored subdirectory is
# asserted against the real shipped client in `tests/hf_client_integration.py`,
# which drives a production acquisition and inspects what the client actually
# created. The hashed basename carries no meaning here beyond being distinct and
# ending in `.incomplete`, which is all the reporter may rely on.
STAGING_ETAG = "deadbeef"


def staged_incomplete_path(output, relative, attempt=None):
    """The staging file the client writes `relative`'s arriving bytes into.

    `attempt` distinguishes the per-process staging names the current client
    uses, so one file can legitimately have more than one staging file on disk.
    """
    metadata = Path(output, ".cache", "huggingface", "download", relative)
    short_hash = hashlib.sha1(metadata.name.encode()).hexdigest()[:8]
    suffix = "" if attempt is None else f".{attempt:08x}"
    staged = metadata.with_name(f"{short_hash}.{STAGING_ETAG}{suffix}.incomplete")
    staged.parent.mkdir(parents=True, exist_ok=True)
    return staged


def byte_events(stream):
    return [
        event
        for event in map(json.loads, stream.getvalue().splitlines())
        if event.get("unit") == "bytes"
    ]


def file_events(stream):
    return [
        event
        for event in map(json.loads, stream.getvalue().splitlines())
        if event.get("unit") == "files"
    ]


class InFlightProgressTests(unittest.TestCase):
    """What a running acquisition reports while large files are in flight.

    Every assertion here is against a known transferred amount: the tests stage
    an exact number of bytes and require the reported figure to be that number,
    rather than requiring that some event was emitted.
    """

    def test_in_flight_bytes_of_a_file_in_a_subdirectory_are_counted(self):
        stream = io.StringIO()
        reporter = hf_fetch.ProgressReporter(stream=stream, minimum_interval=0)
        with tempfile.TemporaryDirectory() as output:
            reporter.set_expected(
                output, [("config.json", 4), ("onnx/model.onnx", 1024)]
            )
            Path(output, "config.json").write_bytes(b"cfg\n")
            staged_incomplete_path(output, "onnx/model.onnx").write_bytes(b"x" * 600)
            reporter._report_files()

        reported = byte_events(stream)
        self.assertEqual(reported[-1]["completed"], 4 + 600)
        self.assertEqual(reported[-1]["total"], 4 + 1024)
        # One file is complete; the 600 bytes come from the file still in flight
        # in a subdirectory, so they cannot be confused with a completion.
        self.assertEqual(file_events(stream)[-1]["completed"], 1)

    def test_reported_bytes_follow_arriving_bytes_not_file_completions(self):
        """The production reporting interval must not hide arriving bytes.

        The reporter is built with the interval the helper actually runs with, so
        a selection dominated by a few large files has to report the bytes that
        arrive within one interval rather than only what completes. Both large
        files here are in a subdirectory, which is where the under-reporting of
        Issue 0082 lives.
        """
        stream = io.StringIO()
        reporter = hf_fetch.ProgressReporter(stream=stream, minimum_interval=5.0)
        chunk = 1 << 16
        expected = [
            ("config.json", 4),
            ("weights/shard-00001-of-00002.safetensors", 3 * chunk),
            ("weights/shard-00002-of-00002.safetensors", 2 * chunk),
        ]
        total = 4 + 5 * chunk
        arriving = []
        with tempfile.TemporaryDirectory() as output:
            reporter.set_expected(output, expected)
            progress = reporter.tqdm_class()(total=total, unit="B")
            Path(output, "config.json").write_bytes(b"cfg\n")
            arrived = 4
            arriving.append(arrived)
            progress.update(4)
            for relative, size in expected[1:]:
                staged = staged_incomplete_path(output, relative)
                staged.write_bytes(b"")
                written = 0
                while written < size:
                    step = min(chunk, size - written)
                    with staged.open("ab") as handle:
                        handle.write(b"x" * step)
                    written += step
                    arrived += step
                    arriving.append(arrived)
                    progress.update(step)
                published = Path(output, relative)
                published.parent.mkdir(parents=True, exist_ok=True)
                published.write_bytes(staged.read_bytes())
                staged.unlink()
                progress.update(0)
            progress.close()

        reported = [event["completed"] for event in byte_events(stream)]
        # Exactly the amounts staged, in order, with the repeats a completion
        # produces collapsed: the reported figure is the bytes on disk and
        # nothing else.
        self.assertEqual(reported, [0] + arriving)
        self.assertEqual(reported[-1], total)
        self.assertTrue(all(value <= total for value in reported), reported)
        # Between the two file completions the figure moved. 4 + 3 * chunk is the
        # first shard's completion; 4 + 4 * chunk is the second shard half-way
        # through, before anything else completed.
        self.assertIn(4 + 3 * chunk, reported)
        self.assertIn(4 + 4 * chunk, reported)
        self.assertLess(
            reported.index(4 + 3 * chunk), reported.index(4 + 4 * chunk)
        )
        self.assertEqual(file_events(stream)[-1]["completed"], 3)

    def test_reported_bytes_never_exceed_the_total(self):
        """Staging left behind by an earlier attempt cannot inflate the figure.

        The current client stages each attempt under its own name, so a retried
        transfer can leave more staged bytes on disk than the file is long. The
        reported figure is a fraction of a known total and must stay one.
        """
        stream = io.StringIO()
        reporter = hf_fetch.ProgressReporter(stream=stream, minimum_interval=0)
        with tempfile.TemporaryDirectory() as output:
            reporter.set_expected(output, [("weights/model.bin", 10)])
            Path(output, "weights").mkdir(parents=True, exist_ok=True)
            Path(output, "weights", "model.bin").write_bytes(b"0123456789")
            for attempt in range(2):
                staged_incomplete_path(
                    output, "weights/model.bin", attempt=attempt
                ).write_bytes(b"0123456789")
            reporter._report_files(force=True, finalized=True)

        reported = [event["completed"] for event in byte_events(stream)]
        self.assertTrue(all(value <= 10 for value in reported), reported)
        self.assertEqual(reported[-1], 10)

    def test_unchanged_in_flight_bytes_are_not_reported_as_fresh_progress(self):
        """A stalled transfer must stay visibly stalled.

        A transfer that has staged bytes but is not receiving any reports the
        same figure, and a repeated identical figure is not progress: it must not
        be emitted again, or a stalled acquisition would be indistinguishable
        from a slow one.
        """
        stream = io.StringIO()
        reporter = hf_fetch.ProgressReporter(stream=stream, minimum_interval=0)
        with tempfile.TemporaryDirectory() as output:
            reporter.set_expected(output, [("onnx/model.onnx", 64)])
            staged_incomplete_path(output, "onnx/model.onnx").write_bytes(b"x" * 7)
            reporter._report_files()
            reporter._report_files()
            reporter._report_files()

        reported = [event["completed"] for event in byte_events(stream)]
        self.assertEqual(reported, [0, 7])

    def test_staging_that_disappears_mid_report_keeps_the_bytes_already_counted(self):
        """A staging file renamed into place while the reporter reads the tree.

        The client publishes a completed file by renaming its staging file, so a
        staging path can vanish between being listed and being measured. That is
        an ordinary race, not a reason to report zero bytes in flight and make a
        progressing transfer look like it went backwards.
        """
        stream = io.StringIO()
        reporter = hf_fetch.ProgressReporter(stream=stream, minimum_interval=0)
        with tempfile.TemporaryDirectory() as output:
            reporter.set_expected(
                output, [("a/first.bin", 100), ("b/second.bin", 100)]
            )
            staged_incomplete_path(output, "a/first.bin").write_bytes(b"x" * 40)
            vanishing = staged_incomplete_path(output, "b/second.bin")
            vanishing.write_bytes(b"x" * 30)
            original_stat = Path.stat

            def stat_with_a_vanishing_file(self, *args, **kwargs):
                if self == vanishing:
                    vanishing.unlink()
                return original_stat(self, *args, **kwargs)

            Path.stat = stat_with_a_vanishing_file
            try:
                reporter._report_files()
            finally:
                Path.stat = original_stat

        reported = [event["completed"] for event in byte_events(stream)]
        # The 40 staged bytes of the file that is still there are still counted.
        self.assertEqual(reported[-1], 40)


if __name__ == "__main__":
    unittest.main()
