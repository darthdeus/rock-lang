import json
import os
from pathlib import Path
import subprocess
from tempfile import TemporaryDirectory
import unittest

from explain_runtime import compare, load
from runtime_pairs import sha256_file
from site_diff import read_capture


MEASUREMENT_VM = os.getenv("ROCK_MEASUREMENT_LUAJIT")


class ExplainTests(unittest.TestCase):
    def test_allocation_and_abort_moves(self):
        base = {
            "traces": {"loop": {("stop", 10, 2, 4, ("loop",)): 1},
                       "loop > body": {("stop", 6, 2, 4, ("body",)): 1}},
            "aborts": {"loop > body": {("depth", "rank", 8, 4, 5, ("rank",)): 4}},
            "allocations": {"movement.rock:20 (closure)": {
                ("interpreter", ""): (4, 80), ("restored", "loop"): (6, 120)}},
            "costs": {"body": (5, 0, 3, 40, 20, 8)},
            "compiler": {},
        }
        candidate = {
            "traces": {"loop": {("stop", 10, 2, 4, ("loop",)): 1},
                       "loop > body": {("stop", 9, 2, 4, ("body",)): 1}},
            "aborts": {"loop > body": {("length", "guard", 15, 502, 5, ("rank", "guard")): 4}},
            "allocations": {"movement.rock:20 (closure)": {("trace", "loop > body"): (10, 200)}},
            "costs": {"body": (5, 0, 3, 20, 40, 10)},
            "compiler": {},
        }
        report = "\n".join(compare(base, candidate, 5))
        for evidence in ("1 changed", "depth at rank", "length at guard",
                         "502 constants", "restored 6 -> 0", "trace 0 -> 10",
                         "body: 40 -> 20 interpreted", "20 -> 40 compiled"):
            self.assertIn(evidence, report)

    def test_source_lineage_survives_shifted_lua_and_trace_ids(self):
        with TemporaryDirectory() as directory:
            root = Path(directory)
            vm = root / "vm"
            vm.touch()
            arms = []
            for offset, generation, exitno in ((0, 1, 6), (10, 9, 8)):
                run = root / str(generation)
                run.mkdir()
                lua = run / "main.lua"
                lua.write_text("\n" * offset + "function loop() end\nfunction side() end\n")
                manifest = run / "manifest.txt"
                manifest.write_text(f"pc-file\t0\t{run}/loop.rock\npc-body\tloop\npc-line\t1\tx\t0\t2\n"
                                    "pc-body\tside\npc-line\t1\tx\t0\t9\n")
                capture = run / "perf-summary.json.sites.tsv"
                capture.write_text(
                    "rock-sites-v2\t0\n"
                    f"site\t1\t{1 + offset}\t{1 + offset}\t{offset}\t0\t1\tmain.lua\n"
                    f"site\t2\t{2 + offset}\t{2 + offset}\t{offset}\t0\t1\tmain.lua\n"
                    f"attempt\t{generation}\t1\t0\t0\t1\t-1\t4\t2\t1\t0\t0\t1\n"
                    f"attempt\t{generation + 1}\t2\t{generation}\t{exitno}\t2\t-1\t5\t2\t1\t0\t0\t2\n"
                    f"trace\t{generation}\t1\t0\t0\t0\t4\t48\t0\n"
                    f"trace\t{generation + 1}\t2\t{generation}\t{exitno}\t0\t5\t64\t0\n")
                hashes = {f"{key}_sha256": sha256_file(path)
                          for key, path in (("vm", vm), ("lua", lua), ("manifest", manifest),
                                            ("capture", capture))}
                (run / "allocation-capture.json").write_text(json.dumps(
                    dict(hashes, frames=2, requests=0, bytes=0, source_roots={str(run): ""})))
                arms.append(load(run, vm))
        self.assertEqual(arms[0]["traces"], arms[1]["traces"])
        self.assertIn("loop [loop.rock:2] > side [loop.rock:9]", arms[0]["traces"])

    @unittest.skipUnless(MEASUREMENT_VM, "requires ROCK_MEASUREMENT_LUAJIT")
    def test_native_capture(self):
        with TemporaryDirectory() as directory:
            captures = []
            for repeat in range(2):
                path = Path(directory) / f"capture{repeat}.tsv"
                subprocess.run([MEASUREMENT_VM, Path(__file__).with_name(
                    "test_measurement_explain.lua"), path, "1"], check=True)
                captures.append(path.read_bytes())
            self.assertEqual(*captures)
            attempts, costs = {}, []
            sites, traces, _, counts = read_capture(path, attempts=attempts, costs=costs)
            self.assertTrue(any(row.parent for row in attempts.values()))
            self.assertTrue(any(len(row.path) >= 3 for row in attempts.values()))
            self.assertTrue(any(kind == 0 and site for site, kind, *rest in counts))
            self.assertTrue(any(kind == 1 and sites[site][0] and traces[trace][1]
                                for site, kind, trace, *_ in counts if site and trace))
            self.assertTrue(any(kind == 6 and trace in attempts
                                for _, kind, trace, *_ in counts))
            lines = Path(__file__).with_name("test_measurement_explain.lua").read_text().splitlines()
            by_line = {sites[site][1]: row for site, *row in costs if site}
            interpreted = lines.index("local function interpreted()") + 1
            boxed = lines.index("local function boxed(i)") + 1
            self.assertEqual(by_line[interpreted], [0, 0, 0, 55, 0, 0])
            self.assertGreater(by_line[boxed][4], 0)
            self.assertEqual(sum(by_line[boxed][3:5]), 72 * 5 * 3)
