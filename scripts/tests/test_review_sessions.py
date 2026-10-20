import datetime as dt
import importlib.util
import json
from pathlib import Path
import tempfile
import subprocess
import sys
import unittest

spec = importlib.util.spec_from_file_location("review_sessions", Path(__file__).resolve().parents[1] / "review-sessions.py")
review = importlib.util.module_from_spec(spec)
spec.loader.exec_module(review)


class SessionReviewTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.home = Path(self.temp.name)
        self.session = self.home / "sessions" / "s1"
        self.session.mkdir(parents=True)
        self.path = self.session / "events.jsonl"
        self.start = review.timestamp("2026-10-20T00:00:00+08:00")
        self.end = self.start + dt.timedelta(days=1)

    def write(self, rows):
        self.path.write_text("".join(json.dumps(dict(type=kind, at=at, seq=n, run_id="r1", data=data)) + "\n" for n, (kind, at, data) in enumerate(rows, 1)))

    def test_cross_day_results_keep_tool_names_without_counting_old_runs(self):
        self.write([
            ("run.accepted", "2026-10-19T15:59:00Z", {}),
            ("tool.planned", "2026-10-19T15:59:01Z", {"call_id": "c1", "plan": {"tool": "bash"}}),
            ("tool.result", "2026-10-19T16:01:00Z", {"call_id": "c1", "status": "completed", "elapsed_ms": 120000, "preview": "已转后台 #a"}),
            ("run.completed", "2026-10-20T16:00:00Z", {}),
        ])
        result = review.collect(self.home, self.start, self.end)
        self.assertEqual(result["accepted_runs"], 0)
        self.assertEqual(result["sessions"], 1)
        self.assertEqual(result["signals"]["bash_detached"], 1)
        self.assertNotIn("run.completed", result["events"])
        self.assertEqual(result["evidence"]["long_tools"][0]["name"], "bash")

    def test_absorbed_wait_is_separate_from_claim_queue_and_unknown_tokens(self):
        self.write([
            ("run.accepted", "2026-10-20T01:00:00Z", {}),
            ("run.absorbed", "2026-10-20T01:02:00Z", {"into": "parent"}),
            ("message.assistant", "2026-10-20T01:03:00Z", {"input_tokens": 0}),
        ])
        result = review.collect(self.home, self.start, self.end)
        self.assertIsNone(result["signals"]["queue_p95_s"])
        self.assertEqual(result["evidence"]["interjections"][0]["duration_s"], 120)
        self.assertEqual(result["tokens"]["input_tokens"], 0)
        self.assertIsNone(result["tokens"]["reasoning_tokens"])

    def test_full_error_classification_exports_no_bodies_or_arguments(self):
        (self.session / "out.json").write_text(json.dumps({"body": {"result": {"error": "AttributeError: has no attribute 'fromtimestamp' secret-content"}}}))
        self.write([
            ("run.accepted", "2026-10-20T01:00:00Z", {"text": "private-request"}),
            ("tool.planned", "2026-10-20T01:00:01Z", {"call_id": "c1", "plan": {"tool": "codemode", "args": {"key": "private-argument"}}}),
            ("tool.result", "2026-10-20T01:00:02Z", {"call_id": "c1", "status": "failed", "output_ref": {"path": "out.json"}}),
        ])
        result = review.collect(self.home, self.start, self.end)
        self.assertEqual(result["signals"]["codemode_failure_categories"], {"datetime_api": 1})
        for secret in ("private-request", "private-argument", "secret-content"):
            self.assertNotIn(secret, json.dumps(result))

    def test_trailing_append_is_visible_and_interior_corruption_is_fatal(self):
        self.write([("run.accepted", "2026-10-20T01:00:00Z", {})])
        original = self.path.read_text()
        self.path.write_text(original + '{"at":')
        self.assertEqual(review.collect(self.home, self.start, self.end)["unfinished_jsonl_tails"], 1)
        self.path.write_text(original + '{"at":\n' + original)
        with self.assertRaisesRegex(ValueError, "invalid JSONL"):
            review.collect(self.home, self.start, self.end)

    def test_regeneration_refreshes_stats_and_keeps_human_notes(self):
        self.write([("run.accepted", "2000-01-02T01:00:00Z", {})])
        output = self.home / "reports"
        command = [sys.executable, "-B", str(Path(__file__).resolve().parents[1] / "review-sessions.py"), "--date", "2000-01-02", "--home", str(self.home), "--output", str(output)]
        subprocess.run(command, check=True, capture_output=True)
        notes = output / "2000-01-02.md"
        notes.write_text("human conclusions stay unchanged")
        (output / "2000-01-02.stats.md").write_text("stale stats")
        subprocess.run(command, check=True, capture_output=True)
        self.assertEqual(notes.read_text(), "human conclusions stay unchanged")
        self.assertNotEqual((output / "2000-01-02.stats.md").read_text(), "stale stats")

    def test_recall_denominator_ignores_old_records_without_diagnostics(self):
        self.write([
            ("memory.injected", "2026-10-20T01:00:00Z", {"uses": []}),
            ("memory.injected", "2026-10-20T01:01:00Z", {"uses": [], "recall": {"mode": "hybrid", "degraded": True}}),
        ])
        result = review.collect(self.home, self.start, self.end)
        self.assertEqual(result["signals"]["memory_injections"], 2)
        self.assertEqual(result["signals"]["memory_with_mode"], 1)
        self.assertEqual(result["signals"]["memory_degraded"], 1)


if __name__ == "__main__":
    unittest.main()
