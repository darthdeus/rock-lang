import json
from pathlib import Path
import shutil
from tempfile import TemporaryDirectory
import unittest

from join_allocation_sites import join, match_source
from runtime_pairs import sha256_file


class JoinAllocationTests(unittest.TestCase):
    def test_operation_filters_and_ambiguous_locations(self):
        first = ("a.rock:1", "vec-construction")
        second = ("b.rock:2", "struct-construction")
        projection = ("a.rock:3", "projection")
        census = {("f", 2): [first, second]}
        sources = {("f", 2): [projection],
                   ("helper", 0): [("type.rock:1", "generated-value-bridge")]}
        self.assertEqual(match_source("f", 2, "TDUP", census, sources), ("", "unknown"))
        self.assertEqual(match_source("f", 2, "TGETS", census, sources), projection)
        self.assertEqual(match_source("helper", 3, "TDUP", census, sources),
                         ("type.rock:1", "generated-table-bridge"))
        self.assertEqual(match_source("helper", 3, "CALL", census, sources),
                         ("type.rock:1", "generated-cdata-bridge"))
        self.assertEqual(match_source("helper", 3, "FNEW", census, sources), ("", "unknown"))

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
                ("a.rock:3", "struct-construction"): (5, 80),
                ("", "vm-compiler"): (4, 128),
                ("", "unknown"): (1, 16),
            })
            manifest.write_text("")
            with self.assertRaisesRegex(ValueError, "manifest"):
                join(root, vm)
            write_capture(1, 3)
            with self.assertRaisesRegex(ValueError, "bytecode"):
                join(root, vm)
