"""run.py keeps each attempt's sessions and verdict before the scratch dir goes.

    python3 -m unittest discover -s evals -p test_run_traces.py
"""
import json
import tempfile
import unittest
from pathlib import Path

import run


class SaveTraceTest(unittest.TestCase):
    def test_sessions_and_verdict_survive_the_scratch_dir(self):
        with tempfile.TemporaryDirectory() as tmp:
            work = Path(tmp) / "wseval-fix-bug"
            session = work / ".worksmith-home" / "sessions" / "2026" / "11" / "14" / "abc"
            session.mkdir(parents=True)
            (session / "transcript.jsonl").write_text('{"type":"meta"}\n')
            dest = Path(tmp) / "traces" / "fix-bug-raw-1"

            saved = run.save_trace(work, dest, {"task": "fix-bug", "passed": False})

            self.assertEqual(saved, dest)
            copied = dest / "sessions" / "2026" / "11" / "14" / "abc" / "transcript.jsonl"
            self.assertTrue(copied.is_file())
            self.assertFalse(json.loads((dest / "verdict.json").read_text())["passed"])

    def test_an_attempt_that_never_started_a_session_saves_nothing(self):
        with tempfile.TemporaryDirectory() as tmp:
            dest = Path(tmp) / "traces" / "t-raw-1"
            self.assertIsNone(run.save_trace(Path(tmp) / "empty", dest, {}))
            self.assertFalse(dest.exists())


if __name__ == "__main__":
    unittest.main()
