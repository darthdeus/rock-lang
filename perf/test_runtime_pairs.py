"""Intervals use paired windows and a matching-work A/A reference."""
from pathlib import Path
from tempfile import TemporaryDirectory
import unittest

from runtime_pairs import instructions_per_frame, summarize


def pairs(deltas, work=1):
    def window(value):
        return {"settings": {}, "work": {"ticks": work}, "metrics": {"aborts": value}}
    return [(window(10), window(10 + delta)) for delta in deltas]


class RuntimePairTests(unittest.TestCase):
    def test_interval_must_fit_inside_the_calibration_band(self):
        calibration = summarize(pairs([-2, -1, 0, 0, 1, 2]), True, None)
        for deltas, status in [
            ([0] * 6, "inside A/A band"),
            ([2] * 5 + [3], "unresolved"),
            ([3] * 6, "outside A/A band"),
        ]:
            with self.subTest(status=status):
                result = summarize(pairs(deltas), False, calibration)
                self.assertEqual(result["metrics"]["aborts"]["status"], status)

    def test_equal_arms_with_different_work_cannot_use_the_calibration(self):
        calibration = summarize(pairs([0] * 6), True, None)
        result = summarize(pairs([0] * 6, work=2), False, calibration)
        self.assertEqual(result["metrics"]["aborts"]["status"], "work differs")

    def test_instruction_delta_uses_only_the_measured_frame_boundaries(self):
        with TemporaryDirectory() as directory:
            path = Path(directory) / "instructions"
            path.write_text("100\n1\n2\n100\n200\n400\n800\n")
            report = {"stalls": {"window": {"start_frame": 101}},
                      "measurement": {"completed_frames": 4}}
            self.assertEqual(instructions_per_frame(report, path), 150)
            report["measurement"]["completed_frames"] = 10
            with self.assertRaisesRegex(ValueError, "incomplete"):
                instructions_per_frame(report, path)


if __name__ == "__main__":
    unittest.main()
