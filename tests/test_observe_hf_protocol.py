#!/usr/bin/env python3
import contextlib
import io
import os
import sys
import unittest
from pathlib import Path
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parent))
import observe_hf_protocol


class SafeFailureTest(unittest.TestCase):
    def test_exception_does_not_emit_url_query_or_traceback(self):
        secret = "https://signed.example/object?token=credential"

        def fail():
            raise RuntimeError(secret)

        stderr = io.StringIO()
        with mock.patch.object(sys, "argv", ["observe_hf_protocol.py"]), mock.patch.dict(
            os.environ,
            {"HF_TOKEN": "", "HUGGING_FACE_HUB_TOKEN": ""},
        ), contextlib.redirect_stderr(stderr), self.assertRaises(SystemExit) as raised:
            observe_hf_protocol.main(fail)

        self.assertEqual(raised.exception.code, 1)
        self.assertEqual(stderr.getvalue(), observe_hf_protocol.SAFE_FAILURE)
        self.assertNotIn(secret, stderr.getvalue())
        self.assertNotIn("Traceback", stderr.getvalue())


if __name__ == "__main__":
    unittest.main()
