"""Intervals use paired windows and a matching-work A/A reference."""
from argparse import Namespace
import hashlib
import json
import os
from pathlib import Path
from tempfile import TemporaryDirectory
import subprocess
import unittest
from unittest.mock import patch

from runtime_pairs import INSTRUCTION_PROBE, instructions_per_frame, main, measure, samples, summarize


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

    def test_late_window_uses_only_the_frames_covered_by_the_allocator(self):
        report = {
            "run": dict.fromkeys(("drawable", "sections", "seed", "bloom", "fx_off",
                                  "present_sync", "gpu_scopes")) | {"jit": "on test"},
            "scenario": {"name": "idle", "phase": "steady", "warmup_frames": 20,
                         "scene": "test", "complete": True},
            "measurement": {"termination": "window_complete", "completed_frames": 8},
            "stalls": {"window": {"start_frame": 30}},
            "memory": {"gc_window": {
                "valid": True, "start_frame": 32, "measured_frames": 3,
                "allocation_calls": 5, "reallocation_calls": 2,
                "allocation_split": {"available": True, **{
                    lane: {"request_bytes": size}
                    for lane, size in (("mutator", 50), ("jit_internal", 20), ("gc", 10))
                }},
                "jit_trace": {"available": True, "aborts": 4, "compiles": 3, "flushes": 0},
            }},
            "frame_records": {"columns": ["frame", "frame_ms", "thread_cpu_ms", "present_ms"],
                              "rows": [[i, i, i, i] for i in range(30, 38)]},
            "section_window": {"sections": []},
            "graphics": dict.fromkeys(("draw_calls", "stream_upload_bytes", "retained_upload_bytes"), 0),
        }
        metrics, _, _ = samples(report, 8, [], (32, 3))
        self.assertEqual(metrics["frame_ms_p50"], 35)
        self.assertEqual(metrics["frame_ms_p99"], 36)
        self.assertEqual(metrics["jit_compiles"], 3)
        with self.assertRaisesRegex(ValueError, "allocation window"):
            samples(report, 8, [], (31, 3))
        with TemporaryDirectory() as directory:
            path = Path(directory) / "instructions"
            path.write_text("30\n20\n30\n40\n50\n100\n200\n300\n400\n")
            self.assertEqual(instructions_per_frame(report, path, (32, 3)), 100)

    def test_mutator_requests_exclude_compilation_and_gc(self):
        from runtime_pairs import allocation_requests_by_origin

        with TemporaryDirectory() as directory:
            path = Path(directory) / "sites.tsv"
            path.write_text("rock-sites-v1\t0\n"
                            "count\t0\t0\t0\t0\t2\t20\n"
                            "count\t0\t1\t0\t0\t1\t10\n"
                            "count\t0\t2\t0\t0\t2\t20\n"
                            "count\t0\t4\t0\t0\t99\t0\n"
                            "count\t0\t6\t0\t0\t1\t20\n"
                            "count\t0\t7\t0\t0\t1\t10\n")
            metrics = dict(alloc_requests=7, alloc_bytes=80, mutator_bytes=50, jit_bytes=20)
            self.assertEqual(allocation_requests_by_origin(path, metrics),
                             {"mutator_requests": 5, "jit_requests": 1, "alloc_gc_requests": 1})
            with self.assertRaisesRegex(ValueError, "reconcile"):
                allocation_requests_by_origin(path, metrics | {"alloc_requests": 6})

    def test_candidate_lua_must_match_its_first_run(self):
        with TemporaryDirectory() as directory:
            args = ["runtime_pairs.py", ".", "base", "candidate", "--timing", "--out", str(Path(directory) / "run")]
            with patch("sys.argv", args), patch("runtime_pairs.measure", side_effect=[
                {"lua_sha256": "base", "metrics": {}}, {"lua_sha256": "candidate", "metrics": {}},
                {"lua_sha256": "different", "metrics": {}},
            ]):
                with self.assertRaisesRegex(ValueError, "02-b:.*Lua differs"):
                    main()

    def test_selected_vm_and_instruction_probe_are_passed_to_each_arm(self):
        with TemporaryDirectory() as directory:
            vm = Path(directory) / "luajit"
            vm.write_bytes(b"test runtime")
            vm.chmod(0o755)
            args = ["runtime_pairs.py", ".", "base", "--out", str(Path(directory) / "run"),
                    "--luajit", str(vm), "--timing", "--instructions", "--pairs", "6"]
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

    def test_measurement_frames_override_the_scenario_default(self):
        with TemporaryDirectory() as directory:
            args = Namespace(out=Path(directory), project=".", frames=200,
                             warmup_frames=20, seconds=5, var=[])
            with patch("runtime_pairs.subprocess.check_output", return_value="load"):
                with patch("runtime_pairs.subprocess.run", return_value=Namespace(returncode=1)) as run:
                    with self.assertRaisesRegex(RuntimeError, "exited 1"):
                        measure(args, Path("base"), {"PERF_MEASURE_FRAMES": "600"}, "trial")
            self.assertEqual(run.call_args.kwargs["env"]["PERF_MEASURE_FRAMES"], "200")

    @unittest.skipUnless(os.getenv("ROCK_TEST_DETERMINISTIC_LUAJIT"), "requires deterministic VM")
    def test_measurement_vm_can_arm_and_confirm_root_blacklisting(self):
        fixture = Path(__file__).with_name("test_measurement_churn.lua")
        subprocess.run([os.environ["ROCK_TEST_DETERMINISTIC_LUAJIT"], fixture,
                        fixture.with_name("measurement-churn.lua")],
                       env=os.environ | {"ROCK_PERF_DETERMINISTIC": "1",
                                         "PERF_FIXED_WORK": "0", "ROCK_PERF_CAPTURE_SITES": "0"},
                       check=True)

    @unittest.skipUnless(os.getenv("ROCK_TEST_DETERMINISTIC_LUAJIT"), "requires deterministic VM")
    def test_deterministic_runs_read_a_virtual_cpu_clock(self):
        probe = """
            local before = os.clock() * 2^20
            _G.__rock_allocation_activation_frame_begin()
            _G.__rock_allocation_activation_frame_begin()
            io.write(before, " ", os.clock() * 2^20)
        """
        output = subprocess.check_output(
            [os.environ["ROCK_TEST_DETERMINISTIC_LUAJIT"], "-e", probe],
            env=os.environ | {"LUA_INIT": Path(__file__).with_name("measurement.lua").read_text(),
                              "ROCK_PERF_DETERMINISTIC": "1", "PERF_FIXED_WORK": "1",
                              "ROCK_PERF_CAPTURE_SITES": "0"},
            text=True)
        self.assertEqual(output, "1 32770")

    def test_counters_default_to_the_deterministic_vm_and_fixed_work(self):
        with TemporaryDirectory() as directory:
            root = Path(directory)
            vm = root / "luajit"
            vm.touch()
            vm.chmod(0o755)
            args = ["runtime_pairs.py", ".", "base", "--out", str(root / "run"), "--pairs", "6"]
            row = {"lua_sha256": "same", "settings": {}, "work": {},
                   "metrics": {"alloc_requests": 2, "jit_aborts": 1, "frame_ms_p99": 10}}
            env = {"ROCK_PERF_LUAJIT": str(vm), "PERF_JIT_HEALTH": "1"}
            with patch("sys.argv", args), patch.dict("os.environ", env, clear=True):
                with patch("runtime_pairs.measure", side_effect=lambda *_: dict(row)) as measure:
                    main()
            for call in measure.call_args_list:
                run_env = call.args[2]
                self.assertEqual(run_env["ROCK_LUAJIT"], str(vm.resolve()))
                self.assertEqual(run_env["ROCK_PERF_DETERMINISTIC"], "1")
                self.assertEqual(run_env["PERF_FIXED_WORK"], "1")
                self.assertEqual(run_env["PERF_JIT_HEALTH"], "0")
            summary = json.loads((root / "run" / "summary.json").read_text())
            self.assertEqual(set(summary["metrics"]), {"alloc_requests", "jit_aborts"})

    def test_fixed_work_and_sites_use_the_same_hook_in_both_arms(self):
        with TemporaryDirectory() as directory:
            root = Path(directory)
            vm = root / "luajit"
            vm.touch()
            vm.chmod(0o755)
            args = ["runtime_pairs.py", ".", "base", "--out", str(root / "run"),
                    "--luajit", str(vm), "--fixed-work", "--capture-sites", "--pairs", "6"]
            row = {"lua_sha256": "same", "settings": {}, "work": {}, "metrics": {}}
            with patch("sys.argv", args), patch.dict("os.environ", {}, clear=True):
                with patch("runtime_pairs.measure", return_value=row) as measure:
                    main()
            hook = "\n" + "\n".join(Path(__file__).with_name(name).read_text()
                                     for name in ("measurement.lua", "measurement-churn.lua"))
            self.assertEqual(measure.call_count, 12)
            for call in measure.call_args_list:
                env = call.args[2]
                self.assertEqual(env["PERF_FIXED_WORK"], "1")
                self.assertEqual(env["ROCK_PERF_CAPTURE_SITES"], "1")
                self.assertEqual(env["LUA_INIT"], hook)
            plan = json.loads((root / "run/summary.json").read_text())["plan"]
            self.assertEqual(plan["measurement_hook_sha256"], hashlib.sha256(hook[1:].encode()).hexdigest())

    def test_explain_cost_arms_source_capture_before_the_allocator_lease(self):
        with TemporaryDirectory() as directory:
            root = Path(directory)
            vm = root / "luajit"
            vm.touch()
            vm.chmod(0o755)
            args = ["runtime_pairs.py", ".", "base", "--out", str(root / "run"),
                    "--luajit", str(vm), "--explain-cost", "--mutator-only", "--pairs", "6"]
            row = {"lua_sha256": "same", "settings": {}, "work": {}, "metrics": {}}
            with patch("sys.argv", args), patch.dict("os.environ", {}, clear=True):
                with patch("runtime_pairs.measure", return_value=row) as measure:
                    main()
            options, _, env, _ = measure.call_args.args
            self.assertTrue(options.capture_sites and options.allocation_sources)
            self.assertEqual(env["ROCK_PERF_EXPLAIN_COST"], "1")
            self.assertLess(env["LUA_INIT"].index("luaJIT_rock_explain"),
                            env["LUA_INIT"].index("luaJIT_rock_sites"))
            plan = json.loads((root / "run/summary.json").read_text())["plan"]
            self.assertTrue(plan["explain_cost"])


if __name__ == "__main__":
    unittest.main()
