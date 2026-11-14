"""Stage 2 failure labels: fixed lists, pinned evidence, no network in tests.

    python3 -m unittest discover -s evals -p test_failure_labels.py
"""
import json
import tempfile
import unittest
from pathlib import Path

import failure_labels as L
import failures as F
from test_failures import ev, turn, write_session


def fake(label):
    calls = []

    def ask(evidence):
        calls.append(evidence)
        if isinstance(label, Exception):
            raise label
        return dict(label)

    return ask, calls


GOOD = {"behaviour": "loop_same_action", "causal": "caused",
        "evidence": "called bash", "note": "it repeated the probe"}


class ParseTest(unittest.TestCase):
    def test_a_label_outside_the_fixed_lists_is_refused(self):
        self.assertEqual(L.parse(json.dumps(GOOD))["behaviour"], "loop_same_action")
        with self.assertRaises(ValueError):
            L.parse(json.dumps({**GOOD, "behaviour": "was confused"}))
        with self.assertRaises(ValueError):
            L.parse(json.dumps({**GOOD, "causal": "maybe"}))
        with self.assertRaises(ValueError):
            L.parse("I think it looped.")

    def test_json_wrapped_in_prose_still_parses(self):
        self.assertEqual(L.parse(f"Here you go:\n{json.dumps(GOOD)}\n")["causal"], "caused")


class LabelTest(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.root = Path(self.tmp.name)
        middle = [ev("nudge", reason="you are spinning in circles")]
        middle += [ev("tool_call", name="bash", arguments='{"command":"curl x"}'),
                   ev("tool_result", name="bash", ok=True, output="exit code: 0")] * 30
        write_session(
            self.root, "/Users/x/Projects/pos",
            *turn("make the reader work", "stuck: spent 150000 tokens on this turn without finishing",
                  *middle),
            *turn("so the app works but your curl does not?", "done"),
            *turn("again", None, ev("error", message="reading stream chunk: Connection reset by peer")),
            *turn("last", None),
        )
        self.failures, _ = F.mine(self.root, None, None, None)

    def tearDown(self):
        self.tmp.cleanup()

    def test_steering_is_pinned_even_when_the_tail_cuts_it(self):
        f = next(f for f in self.failures if f["cause"] == "token_budget")
        text = L.excerpt(f, steps=3)
        self.assertIn("THE USER STEPPED IN DURING THE TURN", text)
        self.assertIn("you are spinning in circles", text)
        self.assertIn("earlier trace lines omitted", text)
        # The user's next message is often the best label there is.
        self.assertIn("so the app works but your curl does not?", text)

    def test_request_failures_are_never_sent_to_the_model(self):
        ask, calls = fake(GOOD)
        cache = L.label_all(self.failures, {}, ask, None, log=lambda *_, **__: None)
        self.assertEqual(len(calls), 3, "three votes, all for the token-budget turn")
        (label,) = cache.values()
        self.assertEqual((label["cause"], label["behaviour"]), ("token_budget", "loop_same_action"))

    def test_cached_labels_are_not_asked_again(self):
        ask, calls = fake(GOOD)
        cache = L.label_all(self.failures, {}, ask, None, log=lambda *_, **__: None)
        L.label_all(self.failures, cache, ask, None, log=lambda *_, **__: None)
        self.assertEqual(len(calls), 3)

    def test_votes_that_stay_malformed_are_dropped_not_guessed(self):
        ask, calls = fake(ValueError("behaviour not in the list"))
        cache = L.label_all(self.failures, {}, ask, None, log=lambda *_, **__: None)
        self.assertEqual(len(calls), 6, "each of three votes gets one retry")
        (label,) = cache.values()
        self.assertEqual((label["behaviour"], label["votes"]), ("ambiguous", []))


class DecideTest(unittest.TestCase):
    def vote(self, behaviour, causal="caused"):
        return {**GOOD, "behaviour": behaviour, "causal": causal}

    def test_two_of_three_wins_with_the_agreeing_causal(self):
        label = L.decide([self.vote("loop_same_action", "contributed"),
                          self.vote("ignored_error"),
                          self.vote("loop_same_action", "contributed")])
        self.assertEqual((label["behaviour"], label["causal"]),
                         ("loop_same_action", "contributed"))

    def test_a_three_way_split_is_ambiguous_not_a_coin_toss(self):
        # Measured: the same trace came back kept_going_after_done on one run
        # and explore_without_acting on the next.
        label = L.decide([self.vote("kept_going_after_done"),
                          self.vote("explore_without_acting"),
                          self.vote("loop_same_action")])
        self.assertEqual(label["behaviour"], "ambiguous")
        self.assertIn("explore_without_acting", label["note"])

    def test_report_groups_by_cause_and_behaviour(self):
        labels = [
            {**GOOD, "cause": "step_limit", "model": "vllm/qwen38", "session": "a",
             "turn": 1, "date": "2026-10-31", "prompt": "p"},
            {**GOOD, "cause": "step_limit", "model": "vllm/qwen38", "session": "b",
             "turn": 2, "date": "2026-10-31", "prompt": "p", "causal": "contributed"},
            {**GOOD, "behaviour": "user_changed_course", "cause": "aborted",
             "model": "vllm/qwen38", "session": "c", "turn": 1, "date": "2026-10-31",
             "prompt": "p"},
        ]
        labels.append({**GOOD, "behaviour": "ambiguous", "causal": "", "cause": "aborted",
                       "model": "m", "session": "d", "turn": 1, "date": "2026-10-31",
                       "prompt": "p", "votes": ["a", "b", "c"]})
        text = L.report(labels)
        self.assertIn("4 failures voted on: 3 labeled", text)
        self.assertIn("1 ambiguous", text)
        self.assertIn("step_limit           loop_same_action         caused 1/2", text)
        self.assertIn("aborted              user_changed_course      caused 1/1", text)


if __name__ == "__main__":
    unittest.main()
