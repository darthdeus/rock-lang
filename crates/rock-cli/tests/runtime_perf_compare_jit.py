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
