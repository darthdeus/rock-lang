"""Alternate fresh native LuaJIT processes; keep timing, allocation and trace runs separate."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import statistics
import subprocess

parser = argparse.ArgumentParser()
parser.add_argument("artifacts", type=Path)
parser.add_argument("--runtime", type=Path, required=True, help="gamelibs platform directory")
parser.add_argument("--stone-lib", type=Path, required=True)
parser.add_argument("--case", choices=("scene", "mesh"), default="scene")
parser.add_argument("--corners", action="store_true", help="mesh optional corner-lane stress")
parser.add_argument("--rounds", type=int, default=6)
parser.add_argument("--frames", type=int)
parser.add_argument("--warmup", type=int)
parser.add_argument("--outdir", type=Path, required=True)
args = parser.parse_args()
runtime = args.runtime.resolve()
artifacts = args.artifacts.resolve()
outdir = args.outdir.resolve()
outdir.mkdir(parents=True, exist_ok=True)
env = dict(os.environ)
env.update(ROCK_STONE_LIB=str(args.stone_lib.resolve()), STONE_HEADLESS="1",
           LUA_PATH=f"{runtime}/share/luajit-2.1/?.lua;{runtime}/share/lua/5.1/?.lua;;",
           LUA_CPATH=f"{runtime}/lib/lua/5.1/?.so;;", ROCK_TEST_SEED="42",
           AUDIT_CASE=args.case, AUDIT_CORNERS="1" if args.corners else "0",
           AUDIT_FRAMES=str(args.frames or (2400 if args.case == "scene" else 120)),
           AUDIT_WARMUP=str(args.warmup or (600 if args.case == "scene" else 30)))
env["LD_LIBRARY_PATH"] = ":".join(filter(None, [str(args.stone_lib.resolve().parent),
    str(runtime / "lib"), env.get("LD_LIBRARY_PATH"), env.get("NIX_LD_LIBRARY_PATH")]))
arms = ("fresh", "retained") if args.case == "scene" else ("growth", "early", "reserved")
native_sha256 = hashlib.sha256(args.stone_lib.read_bytes()).hexdigest()
rows = []
for metric in ("time", "alloc", "trace"):
    for pair in range(args.rounds):
        order = arms if pair % 2 == 0 else tuple(reversed(arms))
        for arm in order:
            env.update(AUDIT_METRIC=metric, AUDIT_ARM=arm)
            lua = artifacts / f"{arm if args.case == 'scene' else 'retained'}.lua"
            result = subprocess.run([str(runtime / "bin/luajit"), str(lua)],
                                    env=env, text=True, capture_output=True, timeout=120)
            log = f"{metric}-{pair}-{arm}.log"
            (outdir / log).write_text(result.stdout + result.stderr)
            if result.returncode or f"{args.case.upper() if args.case == 'mesh' else 'PIXEL'}_OK" not in result.stdout:
                raise RuntimeError(f"Failed {log}: {result.stdout}\n{result.stderr}")
            lines = [line.split() for line in result.stdout.splitlines() if line.startswith("AUDIT ")]
            if len(lines) != 1 or lines[0][1:3] != [args.case, metric]:
                raise RuntimeError(f"Invalid audit record in {log}")
            record = lines[0]
            rows.append(dict(metric=metric, pair=pair, arm=arm, frames=int(record[3]),
                median_us=float(record[4]), p95_us=float(record[5]), bytes_per_frame=float(record[6]),
                warmup_aborts=int(record[7]), measured_aborts=int(record[8]), log=log,
                aborts=[line for line in result.stdout.splitlines() if line.startswith(("ABORT ", "SITE "))],
                vm=next(line for line in result.stdout.splitlines() if line.startswith("VM ")),
                graphics=[line for line in result.stderr.splitlines() if 'renderer=' in line]))
            print(args.case, metric, pair, arm, record[4:], flush=True)
            metadata = dict(case=args.case, corners=args.corners, frames=int(env["AUDIT_FRAMES"]), warmup=int(env["AUDIT_WARMUP"]),
                native_sha256=native_sha256,
                build=json.loads((artifacts / "build.json").read_text()), samples=rows)
            (outdir / "results.json").write_text(json.dumps(metadata, indent=2) + "\n")
for arm in arms:
    timing = [r for r in rows if r["arm"] == arm and r["metric"] == "time"]
    allocation = [r["bytes_per_frame"] for r in rows if r["arm"] == arm and r["metric"] == "alloc"]
    print(arm, "median_us", statistics.median(r["median_us"] for r in timing),
          "p95_us", statistics.median(r["p95_us"] for r in timing),
          "bytes_per_frame", statistics.median(allocation))
