import tempfile
import unittest
from pathlib import Path
from types import SimpleNamespace

import hf_fetch


COMMIT_A = "a" * 40


class MovingRefApi:
    def repo_info(self, repo_id, revision, repo_type):
        self.request = (repo_id, revision, repo_type)
        return SimpleNamespace(sha=COMMIT_A)


class HfFetchTests(unittest.TestCase):
    def test_download_is_pinned_to_commit_resolved_before_ref_moves(self):
        api = MovingRefApi()
        download_calls = []

        def download(**kwargs):
            download_calls.append(kwargs)
            self.assertEqual(kwargs["revision"], COMMIT_A)
            Path(kwargs["local_dir"], "config.json").write_bytes(b"commit-a")

        with tempfile.TemporaryDirectory() as output:
            result = hf_fetch.acquire(
                "org/model", "main", output, api=api, download=download
            )

        self.assertEqual(api.request, ("org/model", "main", "model"))
        self.assertEqual(len(download_calls), 1)
        self.assertEqual(result, {"commit": COMMIT_A, "files": ["config.json"]})

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
