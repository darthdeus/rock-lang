import importlib.util
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[3]


def module(name, path):
    spec = importlib.util.spec_from_file_location(name, path)
    result = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(result)
    return result


fixtures = module("perf_fixtures", ROOT / "scripts/tests/test_runtime_perf_compare.py")


class WindowCompilationTest(fixtures.RuntimePerfCompareTest):
    def arms(self, base_counts=(0, 0, 0, 0), candidate_counts=(0, 0, 0, 0)):
        docs = [fixtures.perf_doc("a" * 40, "base", 10),
                fixtures.perf_doc("b" * 40, "candidate", 11)]
        for doc, counts in zip(docs, [base_counts, candidate_counts]):
            doc["memory"]["gc_window"]["start_frame"] = 100
            doc["memory"]["gc_window"]["jit_trace"] = dict(
                available=True, **dict(zip(["starts", "compiles", "aborts", "flushes"], counts)))
        docs[1]["memory"]["gc_window"]["gross_request_kb_per_frame"] = 120
        return docs

    def report(self, docs):
        result = self.run_compare([docs[0]], [docs[1]])
        self.assertEqual(result.returncode, 0, result.stderr)
        import json
        return json.loads(result.stdout)

    def test_quiet_windows_compare_allocation(self):
        report = self.report(self.arms())
        self.assertTrue(report["allocation_comparison"]["comparable"])
        self.assertEqual(report["metrics"]["memory.gc_window.gross_request_kb_per_frame"]["direction"],
                         "candidate-higher")

    def assert_unresolved(self, docs):
        report = self.report(docs)
        self.assertFalse(report["allocation_comparison"]["comparable"])
        for name, row in report["metrics"].items():
            if name.startswith("memory.gc_window.") and "jit_trace" not in name:
                self.assertEqual(row["direction"], "unresolved-jit", name)
        self.assertEqual(report["metrics"]["frame_ms.effective_cpu.mean"]["direction"],
                         "candidate-higher")
        return report

    def test_equal_heavy_compilation_is_unresolved(self):
        self.assert_unresolved(self.arms((100, 80, 20, 0), (100, 80, 20, 0)))

    def test_material_difference_in_light_windows_is_unresolved(self):
        self.assert_unresolved(self.arms((0, 0, 0, 0), (10, 10, 0, 0)))

    def test_aborts_alone_are_heavy_compilation(self):
        self.assert_unresolved(self.arms((30, 0, 30, 0), (30, 0, 30, 0)))

    def test_flush_is_unresolved(self):
        self.assert_unresolved(self.arms((0, 0, 0, 1), (0, 0, 0, 0)))

    def test_missing_window_counts_are_unknown(self):
        docs = self.arms()
        del docs[0]["memory"]["gc_window"]["jit_trace"]
        self.assert_unresolved(docs)

    def test_unavailable_counts_are_unknown(self):
        docs = self.arms()
        docs[0]["memory"]["gc_window"]["jit_trace"] = dict(
            available=False, starts=None, compiles=None, aborts=None, flushes=None)
        self.assert_unresolved(docs)

    def split_arms(self):
        docs = self.arms((100, 80, 20, 0), (200, 100, 100, 1))
        for doc, mutator, internal in zip(docs, [80, 60], [20, 60]):
            gc = doc["memory"]["gc_window"]
            n = gc["measured_frames"]
            gc["allocation_split"] = {"available": True, "basis": "luajit-vmstate"}
            for key, kb, calls in [("mutator", mutator, 200),
                                   ("jit_internal", internal, 100), ("gc", 0, 0)]:
                gc["allocation_split"][key] = dict(request_bytes=kb * 1024 * n,
                    request_kb_per_frame=kb, allocator_calls=calls)
            gc["allocator_calls"] = 300
            gc["gross_request_kb_per_frame"] = mutator + internal
        return docs

    def test_mutator_split_compares_during_compilation_and_flush(self):
        report = self.report(self.split_arms())
        self.assertTrue(report["allocation_comparison"]["comparable"])
        self.assertEqual(report["allocation_comparison"]["direction"], "candidate-lower")
        self.assertEqual(report["metrics"]["memory.gc_window.allocation_split.mutator.request_kb_per_frame"]["direction"],
                         "candidate-lower")
        self.assertEqual(report["metrics"]["memory.gc_window.gross_request_kb_per_frame"]["direction"],
                         "candidate-higher")

    def test_one_missing_split_keeps_compiling_windows_unresolved(self):
        docs = self.split_arms()
        del docs[0]["memory"]["gc_window"]["allocation_split"]
        self.assert_unresolved(docs)

    def test_text_report_handles_an_origin_missing_from_the_legacy_arm(self):
        docs = self.split_arms()
        del docs[0]["memory"]["gc_window"]["allocation_split"]
        result = self.run_compare([docs[0]], [docs[1]], output_format="text")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("COMPARABLE", result.stdout)
        self.assertIn("memory.gc_window.allocation_split.mutator.request_kb_per_frame: unknown", result.stdout)

    def test_split_bytes_must_reconcile_with_gross(self):
        docs = self.split_arms()
        docs[0]["memory"]["gc_window"]["allocation_split"]["mutator"]["request_bytes"] += 1024
        result = self.run_compare([docs[0]], [docs[1]])
        self.assertEqual(result.returncode, 2)
        self.assertIn("split", result.stderr)

    def test_split_calls_must_reconcile_with_gross(self):
        docs = self.split_arms()
        docs[0]["memory"]["gc_window"]["allocation_split"]["mutator"]["allocator_calls"] += 1
        result = self.run_compare([docs[0]], [docs[1]])
        self.assertEqual(result.returncode, 2)
        self.assertIn("split", result.stderr)

    def test_p99_refuses_short_frame_population(self):
        report = self.report(self.split_arms())
        for series in ["logic_cpu", "render_cpu", "effective_cpu", "rendered"]:
            row = report["metrics"][f"frame_ms.{series}.p99"]
            self.assertEqual(row["direction"], "unresolved-samples")
            self.assertEqual(row["baseline_frames"], [120])
            self.assertEqual(row["minimum_frames"], 1000)

    def test_p99_compares_long_frame_population(self):
        docs = self.split_arms()
        for doc in docs:
            doc["frames"] = 1000
            for block in doc["frame_ms"].values():
                block["n"] = 1000
        report = self.report(docs)
        self.assertEqual(report["metrics"]["frame_ms.effective_cpu.p99"]["direction"], "candidate-higher")

    def test_p99_unknown_population_refuses_verdict(self):
        docs = self.split_arms()
        del docs[0]["frame_ms"]["effective_cpu"]["n"]
        report = self.report(docs)
        self.assertEqual(report["metrics"]["frame_ms.effective_cpu.p99"]["direction"], "unresolved-samples")

    def test_negative_counter_rejects_evidence(self):
        docs = self.arms()
        docs[1]["memory"]["gc_window"]["jit_trace"]["compiles"] = -1
        result = self.run_compare([docs[0]], [docs[1]])
        self.assertEqual(result.returncode, 2)
        self.assertIn("compiles", result.stderr)

    def test_window_start_drift_rejects_evidence(self):
        docs = self.arms()
        docs[1]["memory"]["gc_window"]["start_frame"] = 1000
        result = self.run_compare([docs[0]], [docs[1]])
        self.assertEqual(result.returncode, 2)
        self.assertIn("gc_window_start", result.stderr)


if __name__ == "__main__":
    unittest.main()
