"""Cold compile measurements keep Hyperfine's units, failures and uncertainty."""
import importlib.util
import contextlib
import io
import json
import subprocess
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

spec = importlib.util.spec_from_file_location("perf_run", Path(__file__).with_name("run.py"))
run = importlib.util.module_from_spec(spec)
spec.loader.exec_module(run)


def legacy():
    return {"results": [{"mean": 0.2, "stddev": 0.1, "median": 0.2,
                         "min": 0.1, "max": 0.3, "user": 0.04, "system": 0.02,
                         "times": [0.1, 0.3], "exit_codes": [0, 0]}]}


def version_two():
    # Minimal schema-v2 shape exported by Hyperfine 2.0.0.
    return {"schema_version": 2, "results": [{
        "measurements": [{"time_wall_clock": {"value": 0.2, "unit": "second"},
                          "exit_code": 0}],
        "summary": {
            "time_wall_clock": {"unit": "second", "count": 1, "mean": 0.2,
                                "stddev": None, "median": 0.2, "min": 0.2, "max": 0.2},
            "time_user": {"unit": "second", "mean": 0.04},
            "time_system": {"unit": "second", "mean": 0.02},
        }}]}


class HyperfineTests(unittest.TestCase):
    def invoke(self, report=None, returncode=0, error=None):
        self.export = None

        def execute(args, **kwargs):
            self.export = Path(args[args.index("--export-json") + 1])
            self.export.write_text(json.dumps(report) if report is not None else "broken JSON")
            if error:
                raise error
            return subprocess.CompletedProcess(args, returncode, "", "command failed" if returncode else "")

        # Own the retained diagnostic files even when an assertion fails.
        with tempfile.TemporaryDirectory() as tmp:
            with patch.object(run.tempfile, "tempdir", tmp), patch.object(run.subprocess, "run", execute):
                result, error = run.hyperfine("command", 1, 0, {}, ".")
            self.retained = self.export.exists()
            self.diagnostic = self.export.read_text() if self.retained else None
        return result, error

    def test_legacy_seconds(self):
        result, error = self.invoke(legacy())
        self.assertIsNone(error)
        self.assertEqual(result, dict(mean_ms=200, stddev_ms=100, median_ms=200, min_ms=100,
                                      max_ms=300, user_ms=40, system_ms=20, runs=2))
        self.assertFalse(self.retained)

    def test_v2_single_run_has_unknown_uncertainty(self):
        result, error = self.invoke(version_two())
        self.assertIsNone(error)
        self.assertEqual(result, dict(mean_ms=200, stddev_ms=None, median_ms=200, min_ms=200,
                                      max_ms=200, user_ms=40, system_ms=20, runs=1))
        self.assertFalse(self.retained)

    def test_v2_repeated_runs(self):
        report = version_two()
        result = report["results"][0]
        result["measurements"] *= 2
        result["summary"]["time_wall_clock"].update(count=2, stddev=0.05)
        measurement, error = self.invoke(report)
        self.assertIsNone(error)
        self.assertEqual(measurement["runs"], 2)
        self.assertEqual(measurement["stddev_ms"], 50)

    def test_legacy_null_uncertainty_stays_null(self):
        report = legacy()
        report["results"][0]["stddev"] = None
        result, error = self.invoke(report)
        self.assertIsNone(error)
        self.assertIsNone(result["stddev_ms"])

    def test_invalid_exports_are_errors_with_retained_json(self):
        cases = []
        for field, value in (("unit", "millisecond"), ("count", 2),
                             ("mean", None), ("mean", float("nan"))):
            report = version_two()
            report["results"][0]["summary"]["time_wall_clock"][field] = value
            cases.append(report)
        report = version_two()
        del report["results"][0]["summary"]["time_wall_clock"]["stddev"]
        cases.append(report)
        report = version_two()
        report["results"][0]["measurements"][0]["exit_code"] = 1
        cases.append(report)
        report = legacy()
        report["results"][0]["exit_codes"][0] = 1
        cases.append(report)
        report = version_two()
        report["results"][0]["measurements"] = []
        cases.append(report)
        report = version_two()
        report["schema_version"] = 3
        cases.append(report)
        for report in cases:
            with self.subTest(report=report):
                result, error = self.invoke(report)
                self.assertIsNone(result)
                self.assertIn(str(self.export), error)
                self.assertTrue(self.retained)
                self.assertEqual(self.diagnostic, json.dumps(report))

    def test_tool_and_json_failures_retain_export(self):
        for kwargs in ({"returncode": 1}, {},
                       {"error": subprocess.TimeoutExpired("hyperfine", 3600)},
                       {"error": OSError("cannot execute hyperfine")}):
            with self.subTest(kwargs=kwargs):
                result, error = self.invoke(**kwargs)
                self.assertIsNone(result)
                self.assertIn(str(self.export), error)
                self.assertTrue(self.retained)


class RunnerTests(unittest.TestCase):
    def invoke(self, reports, extra=()):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            binary = root / "rock"
            binary.touch()
            argv = ["run.py", "--repo", tmp, "--bin", str(binary), "--corpus", "test=.",
                    "--only", "incremental,lsp", "--dry-run", *extra]
            revision = dict(sha="abc", short="abc", commit_time=0, subject="test", dirty=False)
            stdout = io.StringIO()
            with patch.object(run.sys, "argv", argv), patch.object(run, "revision", return_value=revision), \
                 patch.object(run.shutil, "which", return_value="hyperfine"), \
                 patch.object(run, "run_json", side_effect=reports), contextlib.redirect_stdout(stdout):
                run.main()
            return json.loads(stdout.getvalue()[stdout.getvalue().index("{\n"):])

    def test_failed_and_missing_requested_measurements_are_partial(self):
        row = self.invoke([(None, "exit 1: failed"),
                           ({"candidates": [], "modules": [], "functions": []}, None)])
        self.assertEqual(row["status"], "partial")
        self.assertIn("exit 1", row["corpora"]["test"]["incremental_error"])
        self.assertEqual(row["corpora"]["test"]["lsp"]["modules"], 0)

    def test_empty_incremental_and_failed_lsp_are_partial(self):
        report = dict(init_ms=1, main_bytes=1, edits=[])
        row = self.invoke([(report, None), (None, "exit 2: survey failed")])
        corpus = row["corpora"]["test"]
        self.assertEqual(row["status"], "partial")
        self.assertIn("incremental body", corpus["incomplete"])
        self.assertIn("survey failed", corpus["lsp_error"])

    def test_unexpected_reload_is_visible_and_skip_lsp_is_respected(self):
        def edit(kind, reload):
            return dict(name=kind, kind=kind, reload=reload, samples_ms=[1], mean_ms=1,
                        max_ms=1, cook_mean_ms=1, emit_mean_ms=1, write_mean_ms=1,
                        reinferred=1, overlay_bytes=1)
        report = dict(init_ms=1, main_bytes=1, edits=[edit("body", "full"),
                                                       edit("signature", "full")])
        row = self.invoke([(report, None)], ["--skip-lsp"])
        incremental = row["corpora"]["test"]["incremental"]
        self.assertEqual(incremental["routes"]["body"]["full"]["edits"], 1)
        self.assertEqual(row["status"], "ok")


if __name__ == "__main__":
    unittest.main()
