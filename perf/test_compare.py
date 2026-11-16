"""The comparison reports candidate over base, per paired run."""
import importlib.util
import unittest
from pathlib import Path

spec = importlib.util.spec_from_file_location("perf_compare", Path(__file__).with_name("compare.py"))
compare = importlib.util.module_from_spec(spec)
spec.loader.exec_module(compare)


class PairedRatioTests(unittest.TestCase):
    def test_slower_candidate_has_ratio_above_one(self):
        base = [1.0, 2.0] * 5
        median, low, high = compare.paired_ratio(base, [t * 1.1 for t in base])
        self.assertAlmostEqual(median, 1.1)
        self.assertAlmostEqual(low, 1.1)
        self.assertAlmostEqual(high, 1.1)

    def test_ratios_pair_runs_rather_than_medians(self):
        median, low, high = compare.paired_ratio([1.0, 3.0] * 5, [3.0, 1.0] * 5)
        self.assertLess(low, 1)
        self.assertGreater(high, 1)

    def test_interval_drops_one_extreme_ratio_per_side_at_ten_runs(self):
        ratios = [0.5, 0.9, 0.95, 0.98, 1.0, 1.0, 1.02, 1.05, 1.1, 2.0]
        _, low, high = compare.paired_ratio([1.0] * 10, ratios)
        self.assertEqual((low, high), (0.9, 1.1))

    def test_too_few_runs_for_an_interval_are_rejected(self):
        with self.assertRaises(ValueError):
            compare.paired_ratio([1.0] * 5, [1.0] * 5)


if __name__ == "__main__":
    unittest.main()
