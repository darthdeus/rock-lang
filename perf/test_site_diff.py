import os
from pathlib import Path
import subprocess
from tempfile import TemporaryDirectory
import unittest

from site_diff import differences, joined_sites, read_capture, source_owners


FIXTURE = """
local ffi = require("ffi")
ffi.cdef[[int luaJIT_rock_sites(int mode, const char *path);]]
local sites = ffi.C.luaJIT_rock_sites
jit.opt.start("hotloop=5", "hotexit=2")
assert(sites(1, nil) == 1)
local function interpreted(i)
    return {x = i, y = i}
end
jit.off(interpreted)
local function traced(out, n)
    for i = 1, n do out[i] = {x = i, y = i} end
end
local function sunk(n, target)
    local sum = 0
    for i = 1, n do
        local t = {x = i}
        if i == target then return t end
        sum = sum + t.x
    end
    return sum
end
local function closure(out, n)
    for i = 1, n do out[i] = function() return i end end
end
local out = {}
assert(sites(2, nil) == 1)
for i = 1, 8 do out[i] = interpreted(i) end
traced(out, 400)
for i = 1, 10 do assert(sunk(300, 400) == 45150) end
assert(sunk(300, 300).x == 300)
closure(out, 300)
assert(out[300]() == 300 and out[400].x == 400)
assert(sites(0, assert(arg[1])) == 1)
"""


class SiteDiffTests(unittest.TestCase):
    def test_source_and_trace_ids_are_rebased_for_each_arm(self):
        captures = []
        with TemporaryDirectory() as directory:
            root = Path(directory)
            for offset, trace_id, count in ((0, 7, 5), (3, 14, 12)):
                lua = root / "main.lua"
                lua.write_text("\n" * offset + "function f(x)\n return x\nend\n")
                tsv = root / "sites.tsv"
                tsv.write_text(
                    "rock-sites-v1\t0\n"
                    f"site\t8\t{1 + offset}\t{2 + offset}\t1\t1\t2\tmain.lua\n"
                    f"trace\t{trace_id}\t8\t0\t0\t0\t5\t68\t0\n"
                    f"count\t8\t5\t{trace_id}\t2\t{count}\t0\n"
                )
                captures.append(joined_sites(tsv, source_owners(lua), False))
        self.assertEqual(list(differences([captures[0]], [captures[1]])), [
            ("exit", "f/1", 1, 1, "f/1:1@1", "2", "return x", 5, 12, 7, 0, 0, 0)
        ])

    def test_overflow_cannot_be_compared_as_zero(self):
        with TemporaryDirectory() as directory:
            path = Path(directory) / "sites.tsv"
            path.write_text("rock-sites-v1\t1\n")
            with self.assertRaisesRegex(ValueError, "overflowed"):
                read_capture(path)

    @unittest.skipUnless(os.getenv("ROCK_TEST_DETERMINISTIC_LUAJIT"), "requires deterministic VM")
    def test_uncalled_prototypes_do_not_change_hot_sites(self):
        vm = os.environ["ROCK_TEST_DETERMINISTIC_LUAJIT"]
        fixture = Path(__file__).with_name("test_measurement_hot.lua")
        rows = [subprocess.check_output([vm, fixture, str(pad)]) for pad in (0, 7, 41)]
        self.assertIn(b":start:", rows[0])
        self.assertEqual(rows[0], rows[1])
        self.assertEqual(rows[0], rows[2])

    @unittest.skipUnless(os.getenv("ROCK_TEST_MEASUREMENT_LUAJIT"), "requires measurement VM")
    def test_vm_distinguishes_lua_allocations_sinks_materialization_and_traces(self):
        with TemporaryDirectory() as directory:
            root = Path(directory)
            lua = root / "main.lua"
            tsv = root / "sites.tsv"
            lua.write_text(FIXTURE)
            subprocess.run([os.environ["ROCK_TEST_MEASUREMENT_LUAJIT"], lua, tsv], check=True)
            sites = joined_sites(tsv, source_owners(lua), True)
        events = {(kind, prototype): (count, size)
                  for (kind, prototype, *_), (count, size) in sites.items()}
        self.assertEqual(events["interpreter", "interpreted/1"][0], 16)
        self.assertGreater(events["trace", "traced/2"][0], 700)
        self.assertGreater(events["sunk-ir", "sunk/2"][0], 0)
        self.assertGreater(events["restored", "sunk/2"][1], 0)
        self.assertGreater(events["abort", "closure/2"][0], 0)
        self.assertGreater(events["exit", "sunk/2"][0], 0)


if __name__ == "__main__":
    unittest.main()
