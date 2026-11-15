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
parser.add_argument("--compare", type=Path, help="candidate artifacts; alternate against baseline")
parser.add_argument("--arm", choices=("fresh", "retained", "growth", "early", "reserved"))
parser.add_argument("--runtime", type=Path, required=True, help="gamelibs platform directory")
parser.add_argument("--stone-lib", type=Path, required=True)
parser.add_argument("--case", choices=("scene", "mesh", "segment", "bookkeeping", "parametric", "shader"), default="scene")
parser.add_argument("--corners", action="store_true", help="mesh optional corner-lane stress")
parser.add_argument("--independent-corners", action="store_true", help="mesh distinct per-corner values")
parser.add_argument("--require-no-coalescing", action="store_true",
                    help="fail on candidate trace coalescing aborts (all arms without --compare)")
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
           AUDIT_INDEPENDENT_CORNERS="1" if args.independent_corners else "0",
           AUDIT_FRAMES=str(args.frames or (2400 if args.case == "scene" else 120)),
           AUDIT_WARMUP=str(args.warmup or (600 if args.case == "scene" else 30)))
env["LD_LIBRARY_PATH"] = ":".join(filter(None, [str(args.stone_lib.resolve().parent),
    str(runtime / "lib"), env.get("LD_LIBRARY_PATH"), env.get("NIX_LD_LIBRARY_PATH")]))
arms = ("fresh", "retained") if args.case == "scene" else ("retained",) if args.case in ("bookkeeping", "parametric", "shader") else ("growth", "early", "reserved")
if args.arm:
    if args.arm not in arms:
        parser.error("--arm is incompatible with --case")
    arms = (args.arm,)
if args.compare and len(arms) != 1:
    parser.error("--compare requires --arm")
variants = (("baseline", artifacts), ("candidate", args.compare.resolve())) if args.compare else (("", artifacts),)
native_sha256 = hashlib.sha256(args.stone_lib.read_bytes()).hexdigest()
rows = []
for metric in ("time", "alloc", "trace"):
    for pair in range(args.rounds):
        combinations = tuple((variant, directory, arm) for variant, directory in variants for arm in arms)
        order = combinations if pair % 2 == 0 else tuple(reversed(combinations))
        for variant, directory, arm in order:
            env.update(AUDIT_METRIC=metric, AUDIT_ARM=arm)
            lua = directory / f"{arm if args.case == 'scene' else 'retained'}.lua"
            result = subprocess.run([str(runtime / "bin/luajit"), str(lua)],
                                    env=env, text=True, capture_output=True, timeout=120)
            log = f"{metric}-{pair}-{variant + '-' if variant else ''}{arm}.log"
            (outdir / log).write_text(result.stdout + result.stderr)
            if result.returncode or f"{'PIXEL' if args.case == 'scene' else args.case.upper()}_OK" not in result.stdout:
                raise RuntimeError(f"Failed {log}: {result.stdout}\n{result.stderr}")
            if args.require_no_coalescing and metric == "trace" and variant != "baseline":
                for line in result.stdout.splitlines():
                    if line.startswith("ABORT ") and "NYI: register coalescing too complex" in line:
                        raise RuntimeError(f"Coalescing abort in {log}: {line}")
            lines = [line.split() for line in result.stdout.splitlines() if line.startswith("AUDIT ")]
            if len(lines) != 1 or lines[0][1:3] != [args.case, metric]:
                raise RuntimeError(f"Invalid audit record in {log}")
            record = lines[0]
            rows.append(dict(metric=metric, pair=pair, arm=arm, variant=variant, frames=int(record[3]),
                median_us=float(record[4]), p95_us=float(record[5]), bytes_per_frame=float(record[6]),
                warmup_aborts=int(record[7]), measured_aborts=int(record[8]), log=log,
                aborts=[line for line in result.stdout.splitlines() if line.startswith(("ABORT ", "SITE "))],
                vm=next(line for line in result.stdout.splitlines() if line.startswith("VM ")),
                graphics=[line for line in result.stderr.splitlines() if 'renderer=' in line]))
            print(args.case, metric, pair, variant, arm, record[4:], flush=True)
            metadata = dict(case=args.case, corners=args.corners, independent_corners=args.independent_corners, frames=int(env["AUDIT_FRAMES"]), warmup=int(env["AUDIT_WARMUP"]),
                native_sha256=native_sha256,
                build=json.loads((artifacts / "build.json").read_text()),
                candidate_build=json.loads((args.compare / "build.json").read_text()) if args.compare else None, samples=rows)
            (outdir / "results.json").write_text(json.dumps(metadata, indent=2) + "\n")
for variant, _ in variants:
    for arm in arms:
        timing = [r for r in rows if r["variant"] == variant and r["arm"] == arm and r["metric"] == "time"]
        allocation = [r["bytes_per_frame"] for r in rows if r["variant"] == variant and r["arm"] == arm and r["metric"] == "alloc"]
        print(variant, arm, "median_us", statistics.median(r["median_us"] for r in timing),
              "p95_us", statistics.median(r["p95_us"] for r in timing),
              "bytes_per_frame", statistics.median(allocation))
