import copy
import json
import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import worker


class WorkerTests(unittest.TestCase):
    def setUp(self):
        self.plan = worker.prepare({"tier": "low", "claimed_model": worker.catalog()["models"][0]["id"], "request_model": "gpt-5.4"})

    def test_catalog_and_exact_counts(self):
        self.assertEqual(sorted(t["count"] for t in worker.catalog()["tiers"]), [36, 72, 108])
        self.assertEqual(len(self.plan["jobs"]), 36)
        self.assertEqual(self.plan["retry_budget"], 18)
        self.assertFalse(any(m.get("reference_only") for m in worker.catalog()["models"]))

    def test_long_unknown_answer_is_bounded_without_changing_scoring(self):
        job = self.plan["jobs"][0]
        answer = "新" * 1000
        data = {"type": "response.completed", "response": {"id": "response-1", "status": "completed", "output": [{"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": answer}]}]}}
        sample = worker.handle({"op": "evaluate", "plan": self.plan, "job_id": job["job_id"], "sse": "data: " + json.dumps(data) + "\n\n"})
        self.assertEqual(sample["status"], "ok")
        self.assertEqual(sample["category"], worker.UNKNOWN)
        self.assertLessEqual(len(sample["answer"].encode()), 256)
        self.assertTrue(sample["answer_truncated"])
        sample["job_id"] = job["job_id"]
        original = {**sample, "category": answer}
        self.assertEqual(worker.score(self.plan, [sample]), worker.score(self.plan, [original]))

    def test_payload_preserves_upstream_probe(self):
        job = self.plan["jobs"][0]
        payload = worker.handle({"op": "payload", "plan": self.plan, "job_id": job["job_id"]})
        self.assertEqual(payload, worker.build_payload("gpt", "gpt-5.4", worker.CELLS[job["cell_id"]]))
        self.assertNotIn("tools", payload)

    def test_stream_rejects_truncation_and_refusal(self):
        base = {"op": "evaluate", "plan": self.plan, "job_id": self.plan["jobs"][0]["job_id"]}
        result = worker.handle({**base, "sse": 'data: {"type":"response.output_text.delta","delta":"cat"}\n\n'})
        self.assertEqual(result["error"], "truncated_stream")
        result = worker.handle({**base, "sse": 'data: {"type":"response.refusal.done"}\n\n'})
        self.assertEqual(result["error"], "response_refused")

    def test_no_samples_is_not_model_mismatch(self):
        result = worker.score(self.plan, [])
        self.assertEqual(result["verdict"], "insufficient")
        self.assertEqual(result["valid_samples"], 0)

    def test_failed_requests_are_not_wrong_answers(self):
        failed = [{"status": "error", "job_id": j["job_id"], "category": "wrong"} for j in self.plan["jobs"]]
        self.assertEqual(worker.score(self.plan, failed), worker.score(self.plan, []))

    def test_rejects_duplicate_successes_and_changed_baseline(self):
        result = {"status": "ok", "job_id": self.plan["jobs"][0]["job_id"], "category": "cat"}
        with self.assertRaises(worker.AppError):
            worker.score(self.plan, [result, result])
        changed = copy.deepcopy(self.plan)
        changed["hash"] = "0" * 64
        with self.assertRaises(worker.AppError):
            worker.score(changed, [])

    def test_all_tiers_use_original_scoring(self):
        from collections import Counter, defaultdict
        for tier in ("low", "medium", "high"):
            plan = worker.prepare({"tier": tier, "claimed_model": self.plan["claimed_model"], "request_model": "gpt-5.4"})
            results, counts = [], defaultdict(Counter)
            for job in plan["jobs"]:
                category = worker.PACKAGE["fitted"]["cells"][job["cell_id"]]["categories"][0]
                results.append({"job_id": job["job_id"], "status": "ok", "category": category})
                counts[job["cell_id"]][category] += 1
            t = worker.PACKAGE["tiers"][tier]
            original = worker.score_counts(worker.PACKAGE["fitted"], dict(counts), t["counts"], t["thresholds"], calibrated=worker.calibration_matches(worker.PACKAGE, tier), claimed_model=plan["claimed_model"], completion_ratio=.6)
            self.assertEqual(worker.score(plan, results), original)
            self.assertEqual(original["valid_samples"], len(plan["jobs"]))



class FailureReportTests(unittest.TestCase):
    def test_key_error_reports_missing_key_without_values(self):
        try:
            worker.handle({"op": "payload", "job_id": "j"})
        except Exception as exc:
            self.assertEqual(worker.failure(exc).split(":")[0], "KeyError")
            self.assertIn("'", worker.failure(exc))

    def test_known_errors_keep_their_code(self):
        class Coded(Exception):
            code = "baseline_changed"
        self.assertEqual(worker.failure(Coded()), "baseline_changed")

    def test_other_errors_report_only_the_class_name(self):
        self.assertEqual(worker.failure(ValueError("secret answer text")), "ValueError")


if __name__ == "__main__":
    unittest.main()
