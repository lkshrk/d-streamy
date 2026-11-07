#!/usr/bin/env python3
import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

SCRIPT = Path(__file__).with_name("check-encoder-settings.py")


class EncoderSettingsTests(unittest.TestCase):
    def check(self, got, want):
        with tempfile.TemporaryDirectory() as root:
            root = Path(root)
            for folder, value in [("dump", got), ("fixtures", want)]:
                (root / folder).mkdir()
                if value is not None:
                    (root / folder / "obs_x264.json").write_text(json.dumps(value))
            return subprocess.run(
                [sys.executable, "-I", str(SCRIPT), str(root / "dump"), str(root / "fixtures"), "artifact", "obs_x264"],
                capture_output=True, text=True,
            )

    def fixture(self, kind="captured"):
        return {"id": "obs_x264", "codec": "h264", "kind": kind,
                "defaults": {"bf": 0}, "properties": [{"name": "bf"}]}

    def test_captured_fixture_accepts_exact_defaults_and_properties(self):
        fixture = self.fixture()
        self.assertEqual(self.check(fixture, fixture).returncode, 0)

    def test_captured_fixture_rejects_new_defaults(self):
        got = self.fixture()
        got["defaults"]["new"] = 1
        self.assertNotEqual(self.check(got, self.fixture()).returncode, 0)

    def test_captured_fixture_rejects_new_properties(self):
        got = self.fixture()
        got["properties"].append({"name": "new"})
        self.assertNotEqual(self.check(got, self.fixture()).returncode, 0)

    def test_bool_and_integer_defaults_differ(self):
        got = self.fixture()
        got["defaults"]["bf"] = False
        self.assertNotEqual(self.check(got, self.fixture()).returncode, 0)

    def test_missing_dump_and_missing_fixture_fail(self):
        self.assertNotEqual(self.check(None, self.fixture()).returncode, 0)
        self.assertNotEqual(self.check(self.fixture(), None).returncode, 0)

    def test_source_fixtures_report_partial_verification(self):
        got = self.fixture()
        got["defaults"]["new"] = 1
        result = self.check(got, self.fixture("source"))
        self.assertEqual(result.returncode, 0)
        self.assertIn("source fixtures not fully verified: obs_x264", result.stdout)


if __name__ == "__main__":
    unittest.main()
