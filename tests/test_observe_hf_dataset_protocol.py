#!/usr/bin/env python3
import contextlib
import io
import os
import sys
import unittest
from pathlib import Path
from unittest import mock

import httpx

sys.path.insert(0, str(Path(__file__).resolve().parent))
import observe_hf_dataset_protocol


class SafeFailureTest(unittest.TestCase):
    def test_trace_retains_only_redirect_hostname(self):
        request = httpx.Request("HEAD", "https://huggingface.co/datasets/a/b?secret=yes")
        response = httpx.Response(
            302,
            request=request,
            headers={"location": "https://signed.example/object?token=credential"},
        )
        trace = observe_hf_dataset_protocol.safe_trace_entry(response)

        self.assertEqual(trace["path"], "/datasets/a/b")
        self.assertEqual(trace["redirect_host"], "signed.example")
        self.assertNotIn("location", trace)
        self.assertNotIn("credential", str(trace))

    def test_exception_does_not_emit_url_query_or_traceback(self):
        secret = "https://signed.example/object?token=credential"

        def fail():
            raise RuntimeError(secret)

        stderr = io.StringIO()
        with mock.patch.object(
            sys, "argv", ["observe_hf_dataset_protocol.py"]
        ), mock.patch.dict(
            os.environ,
            {"HF_TOKEN": "", "HUGGING_FACE_HUB_TOKEN": ""},
        ), contextlib.redirect_stderr(stderr), self.assertRaises(SystemExit) as raised:
            observe_hf_dataset_protocol.main(fail)

        self.assertEqual(raised.exception.code, 1)
        self.assertEqual(stderr.getvalue(), observe_hf_dataset_protocol.SAFE_FAILURE)
        self.assertNotIn(secret, stderr.getvalue())
        self.assertNotIn("Traceback", stderr.getvalue())


if __name__ == "__main__":
    unittest.main()
