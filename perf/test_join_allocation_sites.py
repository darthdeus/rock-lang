import json
from pathlib import Path
import shutil
import subprocess
from tempfile import TemporaryDirectory
import unittest

from join_allocation_sites import bridge_callers, by_type, join, match_source, nobox_verdict, read_manifest
from runtime_pairs import sha256_file


class JoinAllocationTests(unittest.TestCase):
    def test_operation_filters_and_ambiguous_locations(self):
        first = ("a.rock:1", "vec-construction", "", "")
        second = ("b.rock:2", "struct-construction", "", "")
        projection = ("a.rock:3", "projection", "", "")
        census = {("f", 2): [first, second]}
        sources = {("f", 2): [projection],
                   ("helper", 0): [("type.rock:1", "generated-value-bridge", "Point", "helper")]}
        unknown = ("", "unknown", "", "")
        self.assertEqual(match_source("f", 2, "TDUP", census, sources), unknown)
        self.assertEqual(match_source("f", 2, "TGETS", census, sources), projection)
        self.assertEqual(match_source("helper", 3, "TDUP", census, sources),
                         ("type.rock:1", "generated-table-bridge", "Point", "helper"))
        self.assertEqual(match_source("helper", 3, "CALL", census, sources),
                         ("type.rock:1", "generated-cdata-bridge", "Point", "helper"))
        self.assertEqual(match_source("helper", 3, "FNEW", census, sources), unknown)

    def test_boxed_types_charge_helpers_to_their_callers(self):
        with TemporaryDirectory() as directory:
            run = Path(directory)
            lua = run / "main.lua"
            lua.write_text("Point_0123abcd_vt = function(x, y)\n return {x = x, y = y}\nend\n"
                           "function f(x)\n local a = Point_0123abcd_vt(x, x)\n"
                           " local b = Point_0123abcd_vt(x, 0)\nend\n")
            (run / "callers.tsv").write_text("Point_0123abcd_vt\t5\t3\nPoint_0123abcd_vt\t6\t1\n")
            manifest = run / "manifest.txt"
            manifest.write_text(
                "  0123456789abcdef\tvalue-construction\tgame.rock:7\tf\t2\t0\tlocal-store\t1\tm::Point\t2\n"
                "  1123456789abcdef\tvalue-construction\tgame.rock:8\tf\t3\t0\treturned\t1\tm::Point\t2\n"
                "pc-source\tPoint_0123abcd_vt\t0\tgenerated-value-bridge\ttype.rock:1\n")
            census, sources = read_manifest(manifest)
            self.assertEqual(sources, {("Point_0123abcd_vt", 0): {
                ("type.rock:1", "generated-value-bridge", "Point", "helper")}})
            callers = bridge_callers(run, sha256_file(lua), census)
            rows = {("type.rock:1", "generated-table-bridge", "Point", "helper"): (8, 320),
                    ("game.rock:9", "value-construction", "Pair", "global-store"): (2, 64),
                    ("game.rock:9", "struct-construction", "", ""): (1, 99)}
            self.assertEqual([(boxed, size, dict(sites)) for boxed, (_, size, sites)
                              in by_type(rows, callers)], [
                ("Point", 320, {("game.rock:7", "generated-table-bridge", "local-store"): [6, 240],
                                ("game.rock:8", "generated-table-bridge", "returned"): [2, 80]}),
                ("Pair", 64, {("game.rock:9", "value-construction", "global-store"): [2, 64]}),
            ])
            with self.assertRaisesRegex(ValueError, "different Lua"):
                bridge_callers(run, "0" * 64, census)

    @unittest.skipUnless(shutil.which("luajit"), "requires LuaJIT")
    def test_bridge_caller_hook_counts_the_window(self):
        with TemporaryDirectory() as directory:
            output = Path(directory) / "callers.tsv"
            program = ("__rock_allocation_activation_frame_begin = function() end\n"
                       "Point_0123abcd_vt = function(x) return {x = x} end\n"
                       "Point_0123abcd_ct = function() return {} end\n"
                       "for frame = 1, 5 do\n"
                       "  __rock_allocation_activation_frame_begin()\n"
                       "  Point_0123abcd_vt(frame)\n"
                       "  Point_0123abcd_ct()\n"
                       "end\n")
            hook = Path(__file__).with_name("bridge_callers.lua").read_text()
            subprocess.run([shutil.which("luajit"), "-e", hook + program], check=True, env={
                "ROCK_BRIDGE_CALLERS": str(output), "ROCK_BRIDGE_WINDOW_START": "2",
                "ROCK_BRIDGE_WINDOW_LENGTH": "3"})
            line = hook.count("\n") + 6
            self.assertEqual(output.read_text(), f"Point_0123abcd_vt\t{line}\t3\n")

    def test_nobox_verdict_restores_the_declaration(self):
        with TemporaryDirectory() as directory:
            root = Path(directory)
            source = root / "types.rock"
            source.write_text("pub value Point { x: float }\nvalue Pair { a: int }\n")
            rock = root / "rock"
            rock.write_text("#!/bin/sh\nif grep -q 'nobox value Pair' " + str(source) + "; then\n"
                            "  echo '[E00] Error: cannot box' >&2\n"
                            "  echo '   ,-[" + str(root) + "/use.rock:4:9]' >&2\n  exit 1\nfi\n")
            rock.chmod(0o755)
            self.assertEqual(nobox_verdict(str(rock), str(root), f"{source}:1"), "accepted")
            self.assertEqual(nobox_verdict(str(rock), str(root), f"{source}:2"),
                             f"rejected at 1 sites, first {root}/use.rock:4: cannot box")
            self.assertEqual(source.read_text(), "pub value Point { x: float }\nvalue Pair { a: int }\n")

    @unittest.skipUnless(shutil.which("luajit"), "requires LuaJIT")
    def test_bytecode_compatibility_and_accounting(self):
        vm = Path(shutil.which("luajit"))
        with TemporaryDirectory() as directory:
            root = Path(directory)
            lua = root / "main.lua"
            lua.write_text("function f()\n return {x = 1}\nend\n")
            manifest = root / "manifest.txt"
            manifest.write_text(
                "  0123456789abcdef\tstruct-construction\ta.rock:3\tf\t2\n"
                "pc-body\tf\npc-file\t0\ta.rock\npc-line\t2\topaque-call\t0\t4\n"
            )
            capture = root / "perf-summary.json.sites.tsv"
            metadata = root / "allocation-capture.json"

            def write_capture(pc, line):
                capture.write_text(
                    "rock-sites-v1\t0\n"
                    f"site\t8\t1\t{line}\t{pc}\t0\t1\tmain.lua\n"
                    "count\t8\t0\t0\t0\t3\t48\n"
                    "count\t8\t1\t0\t0\t2\t32\n"
                    "count\t8\t3\t0\t0\t9\t0\n"
                    "count\t0\t6\t0\t0\t4\t128\n"
                    "count\t0\t2\t0\t0\t1\t16\n"
                )
                metadata.write_text(json.dumps({
                    "lua_sha256": sha256_file(lua),
                    "vm_sha256": sha256_file(vm),
                    "capture_sha256": sha256_file(capture),
                    "manifest_sha256": sha256_file(manifest),
                    "frames": 2, "requests": 10, "bytes": 224,
                }))

            write_capture(1, 2)
            frames, rows = join(root, vm)
            self.assertEqual(frames, 2)
            self.assertEqual(rows, {
                ("a.rock:3", "struct-construction", "", ""): (5, 80),
                ("", "vm-compiler", "", ""): (4, 128),
                ("", "unknown", "", ""): (1, 16),
            })
            manifest.write_text("")
            with self.assertRaisesRegex(ValueError, "manifest"):
                join(root, vm)
            write_capture(1, 3)
            with self.assertRaisesRegex(ValueError, "bytecode"):
                join(root, vm)
