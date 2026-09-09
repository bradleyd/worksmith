import unittest
import subprocess
import tempfile
from pathlib import Path
from unittest.mock import patch
import run

from run import metrics_fields, parse_events


class MetricsTests(unittest.TestCase):
    def test_stats_failures_do_not_skip_validation(self):
        failures = [subprocess.TimeoutExpired("stats", 30),
                    subprocess.CompletedProcess([], 0, "{bad json", ""),
                    subprocess.CompletedProcess([], 0, '{}', ""),
                    subprocess.CompletedProcess([], 1, "", "stats unavailable")]
        for failure in failures:
            with self.subTest(failure=failure), tempfile.TemporaryDirectory() as tmp:
                completed = subprocess.CompletedProcess(
                    [], 0, '{"type":"session_started","id":"session"}', "")
                with patch.object(run, "setup_workdir", return_value=Path(tmp)), \
                     patch.object(run.subprocess, "run", side_effect=[completed, failure]), \
                     patch.object(run, "validate", return_value=True) as validation:
                    row = run.run_one("worksmith", dict(name="test", goal="test", validate="true"),
                                      "raw", None, 240)
                validation.assert_called_once()
                self.assertTrue(row["passed"])
                self.assertIsNone(row["error"])
                self.assertTrue(row["metrics_error"])
                self.assertNotIn("metrics", row)

    def test_headlines_use_worksmith_combined_accounting(self):
        totals = dict(calls=5, tool_calls=3, completion_tokens=100, reasoning_tokens=20,
                      model_ms=5000, prompt_tokens=1000, cached_tokens=800,
                      known_cost_usd=0.25, unpriced_calls=1)
        stats = {"combined": totals, "workers": [{"id": "w1"}], "warnings": []}
        fields = metrics_fields(stats)
        self.assertEqual(fields["model_calls"], 5)
        self.assertEqual(fields["gen_tokens"], 100)
        self.assertEqual(fields["known_cost_usd"], 0.25)
        self.assertEqual(fields["unpriced_calls"], 1)
        self.assertIs(fields["metrics"], stats)

    def test_helpers_do_not_change_context_peak(self):
        result = parse_events('\n'.join([
            '{"type":"session_started","id":"session"}',
            '{"type":"model_metrics","prompt_tokens":100}',
            '{"type":"model_metrics","purpose":"helper","prompt_tokens":9999}'
        ]))
        self.assertEqual(result["session_id"], "session")
        self.assertEqual(result["ctx_peak"], 100)


if __name__ == "__main__":
    unittest.main()
