"""Intervals use paired windows and a matching-work A/A reference."""
import hashlib
import json
from pathlib import Path
from tempfile import TemporaryDirectory
import unittest
from unittest.mock import patch

from runtime_pairs import INSTRUCTION_PROBE, instructions_per_frame, main, summarize


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

    def test_candidate_lua_must_match_its_first_run(self):
        with TemporaryDirectory() as directory:
            args = ["runtime_pairs.py", ".", "base", "candidate", "--out", str(Path(directory) / "run")]
            with patch("sys.argv", args), patch("runtime_pairs.measure", side_effect=[
                {"lua_sha256": "base"}, {"lua_sha256": "candidate"}, {"lua_sha256": "different"},
            ]):
                with self.assertRaisesRegex(ValueError, "02-b:.*Lua differs"):
                    main()

    def test_selected_vm_and_instruction_probe_are_passed_to_each_arm(self):
        with TemporaryDirectory() as directory:
            vm = Path(directory) / "luajit"
            vm.write_bytes(b"test runtime")
            vm.chmod(0o755)
            args = ["runtime_pairs.py", ".", "base", "--out", str(Path(directory) / "run"),
                    "--luajit", str(vm), "--instructions", "--pairs", "6"]
            row = {"lua_sha256": "same", "settings": {}, "work": {}, "metrics": {}}
            with patch("sys.argv", args), patch("sys.platform", "darwin"), patch.dict("os.environ", {}, clear=True):
                with patch("runtime_pairs.measure", return_value=row) as measure:
                    main()
            self.assertEqual(measure.call_count, 12)
            for call in measure.call_args_list:
                env = call.args[2]
                self.assertEqual(env["ROCK_LUAJIT"], str(vm.resolve()))
                self.assertEqual(env["LUA_INIT"], INSTRUCTION_PROBE)
            plan = json.loads((Path(directory) / "run" / "summary.json").read_text())["plan"]
            self.assertEqual(plan["luajit_sha256"], hashlib.sha256(vm.read_bytes()).hexdigest())


if __name__ == "__main__":
    unittest.main()
