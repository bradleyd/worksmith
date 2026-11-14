"""Stage 1 failure mining: causes come from recorded events, never guesses.

    python3 -m unittest discover -s evals -p test_failures.py
"""
import json
import tempfile
import unittest
from pathlib import Path

import failures as F


def ev(kind, ts=1792000000, **data):
    return {"type": "event", "ts": ts, "data": {"type": kind, **data}}


def write_session(root: Path, cwd: str, *events) -> Path:
    d = root / "2026" / "11" / "01" / f"s{len(list(root.rglob('*.jsonl')))}"
    d.mkdir(parents=True)
    path = d / "transcript.jsonl"
    lines = [{"type": "meta", "ts": 1792000000, "data": {"id": d.name, "cwd": cwd}}]
    lines += events
    path.write_text("\n".join(json.dumps(x) for x in lines) + "\n")
    return path


def turn(prompt, outcome, *middle):
    out = [ev("user_message", text=prompt), *middle]
    if outcome is not None:
        out.append(ev("turn_complete", outcome=outcome))
    return out


class CauseTest(unittest.TestCase):
    def test_outcomes_map_to_fixed_causes(self):
        cases = {
            "aborted": "aborted",
            "hit step limit (200)": "step_limit",
            "validation failed: exit code 1\nTraceback": "validation_exhausted",
            "stuck: repeated `bash` 3 times with identical arguments": "repeated_call",
            "stuck: the model returned an empty response": "empty_reply",
            "stuck: the model spent its whole output budget reasoning and never answered":
                "reasoning_overrun",
            "stuck: spent 151691 tokens on this turn without finishing": "token_budget",
            "blocked: rm -rf /": "blocked",
        }
        for outcome, want in cases.items():
            self.assertEqual(F.cause_of(outcome)[0], want, outcome)

    def test_a_turn_without_an_outcome_is_named_by_its_last_error(self):
        # These are harness or environment failures; stage 2 must not ask a
        # model what the agent did wrong in a turn it never got to answer.
        self.assertEqual(
            F.cause_of(None, "reading stream chunk: Connection reset by peer")[0],
            "request_transport")
        self.assertEqual(
            F.cause_of(None, 'LLM HTTP 400 Bad Request: {"error":"System message must be at the beginning."}')[0],
            "request_rejected")
        self.assertEqual(F.cause_of(None, "LLM HTTP 502 Bad Gateway")[0], "request_provider")
        self.assertEqual(F.cause_of(None, None)[0], "incomplete")


class MineTest(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.root = Path(self.tmp.name)

    def tearDown(self):
        self.tmp.cleanup()

    def test_failed_turns_are_found_and_done_turns_are_not(self):
        write_session(
            self.root, "/Users/x/Projects/pos",
            *turn("plan it", "done", ev("usage", completion_tokens=10)),
            *turn("build it", "hit step limit (50)",
                  ev("model_metrics", model="vllm/qwen38"),
                  ev("usage", completion_tokens=500),
                  ev("tool_call", name="read"),
                  ev("tool_result", name="read", ok=True),
                  ev("nudge", reason="you are spinning in circles")),
        )
        failures, totals = F.mine(self.root, None, None, None)
        self.assertEqual(totals["turns"], 2)
        self.assertEqual(len(failures), 1)
        f = failures[0]
        self.assertEqual((f["cause"], f["model"], f["turn"]), ("step_limit", "vllm/qwen38", 2))
        self.assertEqual(f["steps"], 1)
        self.assertEqual(f["edits"], 0)
        # The user's interjection is steering; a harness nudge would not be.
        self.assertEqual(f["steering"], ["you are spinning in circles"])

    def test_only_successful_writes_count_as_edits(self):
        write_session(
            self.root, "/Users/x/Projects/pos",
            *turn("fix it", "aborted",
                  ev("tool_result", name="edit", ok=False),
                  ev("tool_result", name="write", ok=True),
                  ev("nudge", reason="You've made the same `bash` call repeatedly")),
        )
        f = F.mine(self.root, None, None, None)[0][0]
        self.assertEqual((f["edits"], f["failed_tools"]), (1, 1))
        self.assertEqual((f["nudges"], f["steering"]), (1, []))

    def test_a_last_turn_without_an_outcome_may_still_be_running(self):
        write_session(
            self.root, "/Users/x/Projects/pos",
            *turn("one", None, ev("error", message="LLM HTTP 400 Bad Request")),
            *turn("two", None),
        )
        failures, totals = F.mine(self.root, None, None, None)
        self.assertEqual([f["cause"] for f in failures], ["request_rejected"])
        self.assertEqual(totals["open"], 1)

    def test_sources_and_projects_filter(self):
        write_session(self.root, "/private/var/folders/ab/T/eval1",
                      *turn("t", "aborted"))
        write_session(self.root, "/Users/x/.worksmith/worktrees/w1",
                      *turn("t", "aborted"))
        write_session(self.root, "/Users/x/Projects/pos", *turn("t", "aborted"))
        mine = lambda src, proj=None: [  # noqa: E731
            f["source"] for f in F.mine(self.root, None, src, proj)[0]]
        self.assertEqual(sorted(mine(None)), ["eval", "interactive", "worker"])
        self.assertEqual(mine({"interactive"}), ["interactive"])
        self.assertEqual(mine(None, "Projects/pos"), ["interactive"])


if __name__ == "__main__":
    unittest.main()
