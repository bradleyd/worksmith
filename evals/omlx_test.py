"""Tests for the memory arithmetic that decides whether a 19 GB model may load.

Each case here is a bug that actually happened while this logic lived in shell.
"""

import unittest

from omlx import (
    classify_unload,
    holds_a_model,
    parse_free_gb,
    parse_model_size_gb,
    parse_resident_gb,
    required_gb,
)

VM_STAT = """Mach Virtual Memory Statistics: (page size of 16384 bytes)
Pages free:                                31337.
Pages active:                             976543.
Pages inactive:                           976000.
Pages speculative:                         13107.
Pages wired down:                         249036.
"""


class FreeMemory(unittest.TestCase):
    def test_counts_reclaimable_pages_not_just_free_ones(self):
        # 0.5 GB free on a machine with ~15 GB available: free pages alone would
        # refuse every run, and memory_pressure's "86% free" would allow a load
        # that swaps.
        gb = parse_free_gb(VM_STAT)
        self.assertAlmostEqual(gb, (31337 + 976000 + 13107) * 16384 / 1073741824, places=3)
        self.assertGreater(gb, 15)
        self.assertLess(gb, 16)

    def test_reads_the_page_size_rather_than_assuming_4k(self):
        # Apple silicon reports 16384; assuming 4096 understates by 4x and would
        # refuse every phase.
        small = parse_free_gb(VM_STAT.replace("16384 bytes", "4096 bytes"))
        self.assertAlmostEqual(small, parse_free_gb(VM_STAT) / 4, places=3)

    def test_garbage_is_zero_not_a_crash(self):
        self.assertEqual(parse_free_gb(""), 0.0)
        self.assertEqual(parse_free_gb("not vm_stat output"), 0.0)


class ModelSize(unittest.TestCase):
    def test_finds_a_model_by_id_or_name(self):
        body = '{"models": [{"id": "A", "size_gb": 6.7}, {"name": "B", "size_gb": 19.0}]}'
        self.assertEqual(parse_model_size_gb(body, "A"), 6.7)
        self.assertEqual(parse_model_size_gb(body, "B"), 19.0)
        self.assertIsNone(parse_model_size_gb(body, "C"))

    def test_bytes_are_recognised_as_bytes(self):
        # The field name does not say which unit; 19 means GB, 20401094656 does
        # not, and treating the latter as GB would refuse forever.
        body = '[{"id": "A", "size": 20401094656}]'
        self.assertAlmostEqual(parse_model_size_gb(body, "A"), 19.0, places=1)

    def test_shapes_we_have_not_seen_do_not_crash(self):
        for body in ["", "null", "[]", "{}", '{"models": "nope"}', "not json", '[1, 2, 3]']:
            self.assertIsNone(parse_model_size_gb(body, "A"), body)

    def test_finds_a_size_however_it_is_named_or_nested(self):
        # Guessing exact key names returned None against the real server, and
        # the shape is undocumented, so match on what a key means.
        cases = [
            '[{"id": "A", "model_size_bytes": 20401094656}]',
            '[{"id": "A", "sizeGB": 19.0}]',
            '[{"id": "A", "size_str": "19.00GB"}]',
            '[{"id": "A", "info": {"memory_required_gb": 19.0}}]',
        ]
        for body in cases:
            got = parse_model_size_gb(body, "A")
            self.assertIsNotNone(got, body)
            self.assertAlmostEqual(got, 19.0, places=1, msg=body)

    def test_a_bool_is_not_a_size(self):
        self.assertIsNone(parse_model_size_gb('[{"id": "A", "size": true}]', "A"))


class Requirement(unittest.TestCase):
    def test_headroom_is_added_for_the_kv_cache(self):
        # The 27B loads at ~17 GB actual and still needs room to generate.
        self.assertEqual(required_gb(17.0, 19), 20.4)

    def test_falls_back_when_the_server_will_not_say(self):
        # A hand-picked threshold refused a run at 19.7 GB free that would have
        # fit, so the fallback is only for when the API gives us nothing.
        self.assertEqual(required_gb(None, 19), 22.8)
        self.assertEqual(required_gb(0, 19), 22.8)


class Residency(unittest.TestCase):
    PS = "  RSS COMM\n 7340032 /Applications/oMLX.app/Contents/MacOS/omlx-server\n  512000 firefox\n"

    def test_reports_what_omlx_is_holding(self):
        self.assertAlmostEqual(parse_resident_gb(self.PS), 7.0, places=1)

    def test_absent_process_is_zero(self):
        self.assertEqual(parse_resident_gb("  RSS COMM\n 512000 firefox\n"), 0.0)

    def test_a_loaded_model_is_distinguishable_from_an_idle_server(self):
        self.assertTrue(holds_a_model(7.0))
        self.assertFalse(holds_a_model(0.5), "an idle server holds well under a GB")


class UnloadResponses(unittest.TestCase):
    def test_success(self):
        self.assertEqual(classify_unload(200, "{}"), (True, "unloaded"))

    def test_not_loaded_is_the_normal_case_not_a_failure(self):
        # Reporting this as an error taught the reader to skim the line, which
        # is the opposite of what a memory guard wants.
        ok, msg = classify_unload(400, '{"detail":"Model not loaded: Qwen3.5-9B-OptiQ-4bit"}')
        self.assertTrue(ok)
        self.assertEqual(msg, "already free")

    def test_auth_failure_says_so(self):
        ok, msg = classify_unload(401, '{"detail":"Admin authentication required"}')
        self.assertFalse(ok)
        self.assertIn("admin session", msg)

    def test_anything_else_is_reported_verbatim(self):
        # The first version sent this to /dev/null and the phase refused with
        # 7.2 GB still held, saying nothing about why.
        ok, msg = classify_unload(500, "boom")
        self.assertFalse(ok)
        self.assertIn("500", msg)
        self.assertIn("boom", msg)


if __name__ == "__main__":
    unittest.main()
