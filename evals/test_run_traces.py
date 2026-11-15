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


class SnapshotTest(unittest.TestCase):
    def test_a_rebuild_after_the_snapshot_does_not_change_what_runs(self):
        # The 2026-11-14 run: `cargo test` rewrote the shared binary under the
        # last 12 attempts. The run must hold its own copy.
        with tempfile.TemporaryDirectory() as tmp:
            built = Path(tmp) / "target" / "worksmith"
            built.parent.mkdir()
            built.write_text("#!/bin/sh\necho 'worksmith 0.8.0 (old)'\n")
            built.chmod(0o755)

            copy = run.snapshot_binary(built, Path(tmp) / "run")
            built.write_text("#!/bin/sh\necho 'worksmith 0.8.0 (rebuilt)'\n")

            self.assertNotEqual(copy, built)
            self.assertEqual(run.build_info(copy)["version"], "worksmith 0.8.0 (old)")

    def test_build_info_names_the_checkout(self):
        with tempfile.TemporaryDirectory() as tmp:
            fake = Path(tmp) / "worksmith"
            fake.write_text("#!/bin/sh\necho 'worksmith 0.8.0'\n")
            fake.chmod(0o755)
            info = run.build_info(fake)
            self.assertEqual(info["version"], "worksmith 0.8.0")
            self.assertTrue(info["commit"], "the repo's commit is recorded")
            self.assertIn(info["dirty"], (True, False))


class LengthTest(unittest.TestCase):
    def test_the_suite_splits_by_declared_length(self):
        short = {t["name"] for t in run.load_tasks(None, "short")}
        long = {t["name"] for t in run.load_tasks(None, "long")}
        every = {t["name"] for t in run.load_tasks(None)}
        self.assertIn("newsletter-judge", long)
        self.assertNotIn("newsletter-judge", short)
        self.assertEqual(short | long, every, "every task is one or the other")
        self.assertFalse(short & long)


class ConfigTest(unittest.TestCase):
    def test_toml_dump_round_trips_the_real_project_config(self):
        import tomllib
        original = tomllib.loads((run.REPO / ".worksmith" / "config.toml").read_text())
        self.assertEqual(tomllib.loads(run.toml_dump(original)), original)

    def test_force_context_wins_over_a_per_model_window(self):
        import tomllib
        with tempfile.TemporaryDirectory() as tmp:
            cfg = Path(tmp) / "config.toml"
            cfg.write_text('[models."vllm/qwen38"]\ncontext = 131072\ntemperature = 0.6\n')
            run.force_context(cfg, "vllm/qwen38", 24000)
            data = tomllib.loads(cfg.read_text())
            self.assertEqual(data["models"]["vllm/qwen38"]["context"], 24000)
            self.assertEqual(data["models"]["vllm/qwen38"]["temperature"], 0.6, "the rest survives")
            self.assertEqual(data["agent"]["context-limit"], 24000)


if __name__ == "__main__":
    unittest.main()
