#!/usr/bin/env python3
import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

SCRIPT = Path(__file__).with_name("check-trace-drift.py")


class TraceDriftTests(unittest.TestCase):
    def compare(self, old, new):
        with tempfile.TemporaryDirectory() as root:
            root = Path(root)
            for folder, value in [("old", old), ("new", new)]:
                (root / folder / "trace").mkdir(parents=True)
                if value is not None:
                    (root / folder / "trace" / "trace.json").write_text(json.dumps(value))
            return subprocess.run([sys.executable, "-I", str(SCRIPT), str(root / "old"), str(root / "new")],
                                  capture_output=True, text=True)

    def test_identical_metadata_has_no_drift(self):
        result = self.compare({"fps": 30, "video_packets": 150}, {"fps": 30, "video_packets": 150})
        self.assertEqual(result.returncode, 0)
        self.assertNotIn("::warning", result.stdout)

    def test_packet_count_drift_warns(self):
        result = self.compare({"fps": 30, "video_packets": 150}, {"fps": 30, "video_packets": 149})
        self.assertEqual(result.returncode, 0)
        self.assertIn("::warning", result.stdout)
        self.assertIn("video_packets", result.stdout)

    def test_provenance_run_is_not_compared(self):
        result = self.compare({"ci_run": "old"}, {"ci_run": "new"})
        self.assertEqual(result.returncode, 0)
        self.assertNotIn("::warning", result.stdout)

    def test_missing_fresh_trace_fails(self):
        result = self.compare({"fps": 30}, None)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("missing", result.stdout)


if __name__ == "__main__":
    unittest.main()
