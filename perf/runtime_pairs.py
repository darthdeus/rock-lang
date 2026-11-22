#!/usr/bin/env python3
"""Compare fresh, interleaved JIT-on rock perf runs of one workload."""
import argparse
import hashlib
import json
import math
import os
from pathlib import Path
import statistics
import subprocess
import sys

from compare import median_interval


ALLOC_START = 100
COUNTER_PREFIXES = ("alloc_", "jit_", "mutator_")
INSTRUCTION_PROBE = r'''
local frames = tonumber(os.getenv("PERF_DEFAULT_MEASURE_FRAMES"))
if not frames then return end
local ffi = require("ffi")
ffi.cdef[[
    int getpid(void);
    int proc_pid_rusage(int pid, int flavor, void *buffer);
]]
local pid = ffi.C.getpid()
-- Darwin rusage_info_v4 has 37 words, including the UUID. Word 31
-- is ri_instructions. Sample at frame entry, excluding compiler and boot.
local usage = ffi.new("uint64_t[37]")
local span = frames + 1
local ring = ffi.new("double[?]", span)
local frame = 0
local function snapshot()
    frame = frame + 1
    if ffi.C.proc_pid_rusage(pid, 4, usage) == 0 then
        ring[frame % span] = tonumber(usage[31])
    end
end
jit.off(snapshot)
_G.__rock_allocation_activation_frame_begin = snapshot
_G.__rock_allocation_activation_dump = function()
    local output = assert(os.getenv("PERF_OUT"))
    local file = assert(io.open(output .. ".instructions", "w"))
    local first = math.max(1, frame - span + 1)
    file:write(first, "\n")
    for i = first, frame do
        file:write(string.format("%.0f\n", ring[i % span]))
    end
    file:close()
end
'''


def sha256_file(path):
    with path.open("rb") as source:
        return hashlib.file_digest(source, "sha256").hexdigest()


def samples(report, frames, work_names):
    run = report["run"]
    measurement = report["measurement"]
    gc = report["memory"]["gc_window"]
    trace = gc["jit_trace"]
    if (not run["jit"].startswith("on ") or not report["scenario"]["complete"]
            or measurement["termination"] != "window_complete"
            or measurement["completed_frames"] != frames
            or not gc["valid"] or gc["measured_frames"] != frames - ALLOC_START
            or not trace["available"]):
        raise ValueError("expected a complete JIT-on frame and allocation window")

    start = report["stalls"]["window"]["start_frame"]
    records = report["frame_records"]
    frame = records["columns"].index("frame")
    rows = [row for row in records["rows"] if row[frame] > start]
    if len(rows) != frames - 1:
        raise ValueError("incomplete frame records")

    split = gc["allocation_split"]
    metrics = {
        "alloc_requests": gc["allocation_calls"] + gc["reallocation_calls"],
        "alloc_bytes": (sum(split[lane]["request_bytes"] for lane in ("mutator", "jit_internal", "gc"))
                        if split["available"] else round(gc["gross_request_kb_per_frame"] * 1024
                                                         * gc["measured_frames"])),
        "jit_aborts": trace["aborts"],
        "jit_flushes": trace["flushes"],
    }
    if split["available"]:
        metrics["mutator_bytes"] = split["mutator"]["request_bytes"]
        metrics["jit_bytes"] = split["jit_internal"]["request_bytes"]
    for column in ("frame_ms", "thread_cpu_ms", "present_ms"):
        index = records["columns"].index(column)
        values = [row[index] for row in rows]
        if all(v is None for v in values) and column == "thread_cpu_ms":
            continue
        values = sorted(values)
        metrics[f"{column}_p50"] = statistics.median(values)
        for percent in (95, 99):
            metrics[f"{column}_p{percent}"] = values[math.ceil(len(values) * percent / 100) - 1]
    for key in ("draw_calls", "stream_upload_bytes", "retained_upload_bytes"):
        metrics[f"last_frame_{key}"] = report["graphics"][key]
    work = {row["name"]: row["work_units"] for row in report["section_window"]["sections"]}
    scenario = report["scenario"]
    settings = {key: run[key] for key in ("drawable", "jit", "sections", "seed", "bloom",
                                          "fx_off", "present_sync", "gpu_scopes")}
    settings.update({key: scenario[key] for key in ("name", "phase", "warmup_frames", "scene")})
    settings["exact_alloc_bytes"] = split["available"]
    return metrics, {name: work[name] for name in work_names}, settings


def instructions_per_frame(report, path):
    first, *counts = map(int, path.read_text().splitlines())
    start = report["stalls"]["window"]["start_frame"] + 1
    end = start + report["measurement"]["completed_frames"] - 2
    if not (first <= start < end < first + len(counts)):
        raise ValueError("incomplete retired-instruction window")
    before, after = counts[start - first], counts[end - first]
    if not 0 < before < after:
        raise ValueError("invalid Darwin retired-instruction counters")
    return (after - before) / (end - start)


def summarize(pairs, aa, calibration):
    first = pairs[0][0]
    config = first["settings"]
    if any(row["settings"] != config for pair in pairs for row in pair):
        raise ValueError("runtime settings changed during the comparison")
    if calibration and calibration["settings"] != config:
        raise ValueError("A/A and A/B runtime settings differ")

    work = first["work"]
    work_matches = all(row["work"] == work for pair in pairs for row in pair)
    if calibration:
        work_matches = work_matches and calibration["work"] == work
    result = {}
    for key in first["metrics"]:
        arms = [[pair[i]["metrics"][key] for pair in pairs] for i in (0, 1)]
        differences = [b - a for a, b in zip(*arms)]
        delta, low, high = median_interval(differences)
        status = "A/A reference" if aa else "no A/A calibration"
        band = max(map(abs, differences)) if aa else None
        if not work_matches:
            status = "work differs"
        elif calibration:
            band = calibration["metrics"][key]["aa_band"]
            status = "unresolved"
            if -band <= low <= high <= band:
                status = "inside A/A band"
            elif low > band or high < -band:
                status = "outside A/A band"
        result[key] = {
            "base": [min(arms[0]), statistics.median(arms[0]), max(arms[0])],
            "candidate": [min(arms[1]), statistics.median(arms[1]), max(arms[1])],
            "paired_delta": delta, "ci95": [low, high],
            "aa_band": band,
            "status": status,
        }
    return {"settings": config, "work": work, "work_matches": work_matches,
            "metrics": result, "pairs": pairs}


def measure(args, binary, env, name):
    load = subprocess.check_output(["uptime"], text=True).strip()
    print(f"{name}  {load}", flush=True)
    output = args.out / name
    output.mkdir()
    command = [str(binary.resolve()), "perf", args.project, "--format", "json",
               "--frames", str(args.frames), "--warmup-frames", str(args.warmup_frames),
               "--seconds", str(args.seconds), "--gc-window", str(args.frames - ALLOC_START),
               "--gc-window-start", str(ALLOC_START), "--gc-window-mode", "running",
               "--output", str(output), *(f"--var={var}" for var in args.var)]
    env = env | {"PERF_MEASURE_FRAMES": str(args.frames)}
    with (output / "run.log").open("w") as log:
        result = subprocess.run(command, env=env, stdout=log, stderr=subprocess.STDOUT)
    if result.returncode:
        raise RuntimeError(f"{name}: rock perf exited {result.returncode}. See {output / 'run.log'}")
    try:
        report = json.loads((output / "perf-summary.json").read_text())
        metrics, work, settings = samples(report, args.frames, args.work)
        if env.get("ROCK_PERF_DETERMINISTIC") == "1":
            if not settings["exact_alloc_bytes"]:
                raise ValueError("deterministic comparisons require exact allocation bytes")
            if report["memory"]["gc_window"]["collector_running_at_start"]:
                raise ValueError("automatic GC was running in the fixed collection window")
        if args.instructions:
            metrics["retired_instructions_per_frame"] = instructions_per_frame(
                report, output / "perf-summary.json.instructions")
        if args.fixed_work:
            scenario = report["scenario"]
            work.update(ready_frame=scenario["ready_frame"], start_frame=scenario["absolute_start_frame"])
        if args.capture_sites and not (output / "perf-summary.json.sites.tsv").is_file():
            raise ValueError("missing LuaJIT site capture")
    except (OSError, ValueError, KeyError, TypeError) as error:
        raise ValueError(f"{output}: {error}") from error
    return {"metrics": metrics, "work": work, "settings": settings, "uptime": load,
            "lua_sha256": sha256_file(output / "main.lua"),
            "end_uptime": subprocess.check_output(["uptime"], text=True).strip()}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("project")
    parser.add_argument("base", type=Path)
    parser.add_argument("candidate", nargs="?", type=Path, help="omit for A/A calibration")
    parser.add_argument("--out", required=True, type=Path)
    parser.add_argument("--noise", type=Path, help="summary.json from a matching A/A calibration")
    parser.add_argument("--pairs", type=int, default=10)
    parser.add_argument("--frames", type=int, default=600)
    parser.add_argument("--warmup-frames", type=int, default=600)
    parser.add_argument("--seconds", type=int, default=240, help="wall time cap")
    parser.add_argument("--seed", default="42")
    parser.add_argument("--instructions", action="store_true",
                        help="sample frame-window retired instructions on macOS")
    parser.add_argument("--luajit", type=Path, help="use this LuaJIT executable for both arms")
    parser.add_argument("--timing", action="store_true",
                        help="compare timing with the ordinary VM (default: deterministic counters)")
    parser.add_argument("--fixed-work", action="store_true",
                        help="use Stone's fixed image work and reset JIT and GC on frame three")
    parser.add_argument("--capture-sites", action="store_true",
                        help="capture allocator and trace sites in a separate diagnostic run")
    parser.add_argument("--var", action="append", default=[], help="common compiler build var NAME=VALUE")
    parser.add_argument("--work", action="append", default=[], help="require equal work_units for this section")
    args = parser.parse_args()
    if args.pairs < 6 or not ALLOC_START < args.frames <= 65536:
        parser.error("use at least six pairs and 101 to 65536 measured frames")
    if not args.timing:
        args.fixed_work = True
        if not args.luajit:
            configured_vm = os.environ.get("ROCK_PERF_LUAJIT")
            if not configured_vm:
                parser.error("counter comparisons require a deterministic --luajit or ROCK_PERF_LUAJIT")
            args.luajit = Path(configured_vm)
    if args.instructions and (sys.platform != "darwin" or "LUA_INIT" in os.environ):
        parser.error("--instructions requires macOS and an unset LUA_INIT")
    if (args.fixed_work or args.capture_sites) and "LUA_INIT" in os.environ:
        parser.error("measurement hooks require an unset LUA_INIT")
    if args.capture_sites and not args.luajit:
        parser.error("--capture-sites requires a site-enabled --luajit")
    if args.noise and not args.candidate:
        parser.error("--noise requires an A/B comparison")
    calibration = json.loads(args.noise.read_text()) if args.noise else None
    plan = {"frames": args.frames, "warmup_frames": args.warmup_frames, "seed": args.seed,
            "project": args.project, "work": args.work, "instructions": args.instructions,
            "mode": "timing" if args.timing else "counters",
            "vars_sha256": hashlib.sha256(json.dumps(args.var).encode()).hexdigest()}
    if args.luajit:
        args.luajit = args.luajit.resolve()
        if not args.luajit.is_file() or not os.access(args.luajit, os.X_OK):
            parser.error("--luajit requires an executable file")
        plan["luajit_sha256"] = sha256_file(args.luajit)
    if args.instructions:
        plan["instruction_probe_sha256"] = hashlib.sha256(INSTRUCTION_PROBE.encode()).hexdigest()
    if args.fixed_work or args.capture_sites:
        hook = Path(__file__).with_name("measurement.lua").read_text()
        plan.update(fixed_work=args.fixed_work, capture_sites=args.capture_sites,
                    measurement_hook_sha256=hashlib.sha256(hook.encode()).hexdigest())
    if calibration and (not calibration["aa"] or calibration["plan"] != plan
                        or not calibration["work_matches"]):
        parser.error("--noise requires A/A calibration with matching inputs and work")
    args.out = args.out.resolve()
    args.out.mkdir(parents=True)
    env = dict(os.environ, ROCK_TEST_SEED=args.seed, PERF_NO_SECTIONS="1", STONE_PRESENT_SYNC="none",
               PERF_GC_WINDOW_AFTER_WARMUP="0",
               ROCK_PERF_DETERMINISTIC="0" if args.timing else "1",
               PERF_FIXED_WORK=str(int(args.fixed_work)), ROCK_PERF_CAPTURE_SITES=str(int(args.capture_sites)))
    if not args.timing:
        env["PERF_JIT_HEALTH"] = "0"
    if args.luajit:
        env["ROCK_LUAJIT"] = str(args.luajit)
    if args.instructions:
        env["LUA_INIT"] = INSTRUCTION_PROBE
    if args.fixed_work or args.capture_sites:
        env["LUA_INIT"] = env.get("LUA_INIT", "") + "\n" + hook
    aa = args.candidate is None
    binaries = (args.base, args.candidate or args.base)
    references = {0: calibration["pairs"][0][0]["lua_sha256"]} if calibration else {}
    pairs = []
    for pair in range(args.pairs):
        rows = [None, None]
        for arm in ((1, 0) if pair % 2 else (0, 1)):
            name = f"{pair + 1:02}-{'ab'[arm]}"
            rows[arm] = measure(args, binaries[arm], env, name)
            digest = rows[arm]["lua_sha256"]
            source_arm = 0 if aa else arm
            if digest != references.setdefault(source_arm, digest):
                raise ValueError(f"{name}: Lua differs from its reference; freeze sources and build variables")
            metrics = rows[arm]["metrics"]
            rows[arm]["metrics"] = {
                key: value for key, value in metrics.items()
                if (key.startswith(COUNTER_PREFIXES) != args.timing
                    or key.startswith(("last_frame_", "retired_instructions_")))
            }
        pairs.append(rows)

    result = summarize(pairs, aa, calibration)
    result.update(aa=aa, plan=plan)
    (args.out / "summary.json").write_text(json.dumps(result, indent=2) + "\n")
    for key, metric in result["metrics"].items():
        low, high = metric["ci95"]
        band = "" if metric["aa_band"] is None else f"  A/A +/-{metric['aa_band']:.6g}"
        print(f"{key:30} delta {metric['paired_delta']:+.6g} [{low:+.6g}, {high:+.6g}]"
              f"{band}  {metric['status']}")


if __name__ == "__main__":
    main()
