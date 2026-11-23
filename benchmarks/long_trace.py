#!/usr/bin/env python3
"""Compare one wide per-row trace with short passes over the same rows."""

import argparse
import os
from pathlib import Path
import shutil
import subprocess
from tempfile import TemporaryDirectory


def workload():
    width, groups = 22, 8
    fields = [f"cell{i:03}" for i in range(width * groups)]
    yield "@lua_named struct TraceRow {"
    yield from (f"    {field}: int," for field in fields)
    yield "}"
    for group in range(groups):
        part = fields[group * width:(group + 1) * width]
        yield f"fn sum_group_{group}(row: TraceRow) -> int {{"
        yield "    return " + " + ".join(f"row.{field}" for field in part)
        yield "}"
        yield f"""
fn sum_pass_{group}(rows: [TraceRow], count: int) -> int {{
    let sum = 0
    for i in 0..count {{ sum += sum_group_{group}(rows[i % 64]) }}
    return sum
}}"""
    yield """
@nomangle fn setup() -> [TraceRow] {
    let rows: [TraceRow] = []
    for i in 0..64 {
        rows.push(TraceRow {"""
    yield from (f"            {field}: (i + {i}) % 31," for i, field in enumerate(fields))
    yield """        })
    }
    return rows
}

@nomangle fn fused(rows: [TraceRow], count: int) -> int {
    let sum = 0
    for i in 0..count {
        let row = rows[i % 64]"""
    yield from (f"        sum += sum_group_{group}(row)" for group in range(groups))
    yield """    }
    return sum
}

@nomangle fn split(rows: [TraceRow], count: int) -> int {"""
    yield "    return " + " + ".join(f"sum_pass_{group}(rows, count)" for group in range(groups))
    yield "}\n\nfn main() {}"


DRIVER = r'''
assert(jit.status(), "JIT must be on")
local util = require("jit.util")
local traceerr = require("jit.vmdef").traceerr
dofile(arg[1])
local rows = setup()
local trial = assert(_G[arg[2]])
local compiles, constants, overflow, aborts = 0, 0, 0, 0
jit.flush()
jit.attach(function(event, id, _, _, reason)
    if event == "abort" then
        aborts = aborts + 1
        if traceerr[reason] == "trace too long" then overflow = overflow + 1 end
    elseif event == "stop" then
        local trace = util.traceinfo(id)
        if trace then
            compiles = compiles + 1
            constants = math.max(constants, trace.nk)
        end
    end
end, "trace")
trial(rows, 200)
local start = os.clock()
local result = trial(rows, 120000)
print(result, string.format("CPU %.6f s, compiles %d, largest constant count %d, aborts %d, too long %d",
    os.clock() - start, compiles, constants, aborts, overflow))
'''


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--rock", default="rock")
    parser.add_argument("--luajit", default="luajit")
    args = parser.parse_args()
    executable = shutil.which(args.luajit)
    if not executable:
        parser.error(f"cannot find LuaJIT executable {args.luajit}")
    env = os.environ.copy()
    share = Path(executable).absolute().parent.parent / "share" / "luajit-2.1"
    if share.is_dir():
        env["LUA_PATH"] = f"{share}/?.lua;" + env.get("LUA_PATH", ";")
    with TemporaryDirectory(prefix="rock-long-trace-") as directory:
        source, lua, driver = (Path(directory) / f for f in ("trace.rock", "trace.lua", "driver.lua"))
        source.write_text("\n".join(workload()) + "\n", encoding="ascii")
        driver.write_text(DRIVER, encoding="ascii")
        subprocess.run([args.rock, "compile", source, "--output", lua], check=True)
        results = []
        for mode in ("fused", "split"):
            output = subprocess.check_output([executable, driver, lua, mode], env=env, text=True)
            checksum, report = output.strip().split("\t", maxsplit=1)
            results.append(checksum)
            print(f"{mode}: {report} (checksum {checksum})")
        if results[0] != results[1]:
            raise SystemExit("workload checksums differ")


if __name__ == "__main__":
    main()
