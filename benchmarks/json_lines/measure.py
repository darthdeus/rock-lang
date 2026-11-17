#!/usr/bin/env python3
"""Compare two emitted JSON Lines workloads in alternating LuaJIT processes."""

import argparse
import hashlib
import json
from pathlib import Path
import statistics
import subprocess


HERE = Path(__file__).resolve().parent
DRIVER = HERE / "driver.lua"
SIDES = ("baseline", "candidate")
MODES = ("validate", "records")


def sha256(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def run_sample(luajit, emitted, mode, metric, input_jsonl):
    output = subprocess.check_output(
        [luajit, str(DRIVER), str(emitted), mode, metric, str(input_jsonl)],
        text=True,
    )
    result = json.loads(output)
    if result["mode"] != mode or result["metric"] != metric:
        raise ValueError("driver returned the wrong mode or metric")
    if not result["docs"] or not result["input_bytes"]:
        raise ValueError("driver returned an empty workload")
    if metric == "time" and result["docs"] < 20:
        raise ValueError("timing sample processed fewer than 20 documents")
    return result


def metadata(luajit):
    output = subprocess.check_output(
        [luajit, "-e", "io.write(jit.version, '\\n', jit.os, '\\n', jit.arch)"],
        text=True,
    )
    version, vm_os, vm_arch = output.strip().splitlines()
    return {"luajit_version": version, "vm_os": vm_os, "vm_arch": vm_arch}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("baseline", type=Path, help="baseline emitted Lua")
    parser.add_argument("candidate", type=Path, help="candidate emitted Lua")
    parser.add_argument("input_jsonl", type=Path, help="JSON Lines benchmark input")
    parser.add_argument("--luajit", default="luajit")
    parser.add_argument("--pairs", type=int, default=10,
                        help="alternating timing and heap pairs (minimum 10)")
    args = parser.parse_args()
    if args.pairs < 10:
        parser.error("--pairs must be at least 10")

    emitted = {"baseline": args.baseline, "candidate": args.candidate}
    report = {
        "hashes": {
            "baseline_lua_sha256": sha256(args.baseline),
            "candidate_lua_sha256": sha256(args.candidate),
            "input_jsonl_sha256": sha256(args.input_jsonl),
            "driver_lua_sha256": sha256(DRIVER),
        },
        "vm": metadata(args.luajit),
        "settings": {"pairs": args.pairs, "timed_passes": 20,
                     "warmup_passes": 3, "profile_passes": 200,
                     "trace_pairs": args.pairs},
        "samples": [],
    }
    reference_checksums = {}

    for mode in MODES:
        for metric in ("time", "heap"):
            for pair in range(args.pairs):
                order = SIDES if pair % 2 == 0 else tuple(reversed(SIDES))
                for side in order:
                    result = run_sample(args.luajit, emitted[side], mode, metric,
                                        args.input_jsonl)
                    checksum_key = (mode, metric)
                    expected = reference_checksums.setdefault(checksum_key,
                                                              result["checksum"])
                    if result["checksum"] != expected:
                        raise ValueError(f"checksum mismatch for {mode} {metric}")
                    report["samples"].append({
                        "mode": mode,
                        "metric": metric,
                        "pair": pair,
                        "side": side,
                        "docs": result["docs"],
                        "input_bytes": result["input_bytes"],
                        "checksum": result["checksum"],
                        "ns_per_doc": result.get("ns_per_doc"),
                        "mb_per_second": result.get("mb_per_second"),
                        "heap_bytes_per_input_mb": result.get("heap_bytes_per_input_mb"),
                    })

        for side in SIDES:
            for metric, field in (("time", "ns_per_doc"),
                                  ("time", "mb_per_second"),
                                  ("heap", "heap_bytes_per_input_mb")):
                values = [sample[field] for sample in report["samples"]
                          if sample["mode"] == mode and sample["metric"] == metric
                          and sample["side"] == side]
                if len(values) != args.pairs or any(value is None for value in values):
                    raise ValueError(f"missing {mode} {metric} samples for {side}")
                report.setdefault("medians", {})[f"{mode}.{field}.{side}"] = statistics.median(values)

        for pair in range(args.pairs):
            order = SIDES if pair % 2 == 0 else tuple(reversed(SIDES))
            for side in order:
                result = run_sample(args.luajit, emitted[side], mode, "trace", args.input_jsonl)
                expected = reference_checksums.setdefault((mode, "trace"), result["checksum"])
                if result["checksum"] != expected:
                    raise ValueError(f"checksum mismatch for {mode} trace run")
                report["samples"].append({
                    "mode": mode,
                    "metric": "trace",
                    "pair": pair,
                    "side": side,
                    "docs": result["docs"],
                    "input_bytes": result["input_bytes"],
                    "checksum": result["checksum"],
                    "trace": result["trace"],
                })

        for side in SIDES:
            for count in ("starts", "stops", "aborts"):
                values = [sample["trace"][count] for sample in report["samples"]
                          if sample["mode"] == mode and sample["metric"] == "trace"
                          and sample["side"] == side]
                report["medians"][f"{mode}.trace.{side}.{count}"] = statistics.median(values)

    report["checksums_match"] = True
    print(json.dumps(report, indent=2, sort_keys=True))


if __name__ == "__main__":
    main()
