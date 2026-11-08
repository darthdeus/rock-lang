#!/usr/bin/env python3
"""Print a compile's time budget as a tree.

    perf/budget.py packages/stone              # compile once, print the budget
    perf/budget.py packages/stone --runs 5     # the fastest of five compiles
    perf/budget.py --record stats.jsonl        # print the last record of a file

The compiler writes one record per compile when `ROCK_COMPILE_STATS` names a
file. Every node's wall time is the sum of its children (the `other` child is
the remainder), so the percentages are of the whole process and a phase that
grows shows up without anyone having added a timer for it. The emit fan-out
also lists the summed per-unit time of each pass, which overlaps across
workers: compare those with each other and across commits, not with wall.
"""
import argparse
import json
import os
import subprocess
import sys
import tempfile
from pathlib import Path


def measurement_env():
    env = dict(os.environ)
    env.update(ROCK_INVOCATION_LOG="off", ROCK_FRAME_COMPILE_CACHE="0",
               ROCK_SIM_COMPILE_CACHE="0", ROCK_NO_WARNINGS="1")
    return env


def compile_budget(rock, project, runs, env):
    """The record of the fastest of `runs` compiles, or an error string."""
    best = None
    with tempfile.TemporaryDirectory(prefix="rock-budget-") as tmp:
        stats = Path(tmp) / "stats.jsonl"
        out = Path(tmp) / "out.lua"
        for _ in range(runs):
            stats.unlink(missing_ok=True)
            proc = subprocess.run([rock, "compile", project, "--output", str(out)],
                                  env=dict(env, ROCK_COMPILE_STATS=str(stats)),
                                  capture_output=True, text=True, timeout=1800)
            if proc.returncode:
                return None, f"compile failed ({proc.returncode}): {proc.stderr.strip()[-2000:]}"
            if not stats.exists():
                return None, "the compiler wrote no ROCK_COMPILE_STATS record"
            record = json.loads(stats.read_text().splitlines()[-1])
            if record.get("status") != "ok":
                return None, f"record status {record.get('status')}"
            if best is None or record["wall_ms"] < best["wall_ms"]:
                best = record
    return best, None


def render(node, total, depth=0, lines=None):
    lines = [] if lines is None else lines
    pct = 100 * node["wall_ms"] / total if total else 0
    extra = ""
    if "threads" in node:
        extra = f"  on {node['threads']} threads"
    if "unit_cpu_ms" in node:
        extra += f"  unit cpu {node['unit_cpu_ms']:.1f} ms summed over all lowered bodies"
    if "scaffold_ms" in node:
        extra += f"  (scaffold {node['scaffold_ms']:.1f} ms inside)"
    lines.append(f"{'  ' * depth}{node['name']:<32} {node['wall_ms']:9.1f} ms {pct:5.1f}%{extra}")
    for child in node.get("children", []):
        if child["name"] == "other" and child["wall_ms"] < 0.05:
            continue
        render(child, total, depth + 1, lines)
    for branch in node.get("branches", []):
        lines.append(f"{'  ' * (depth + 1)}| {branch['name']:<30} {branch['wall_ms']:9.1f} ms (overlapping lane)")
    for pass_ in node.get("unit_cpu", []):
        lines.append(f"{'  ' * (depth + 1)}~ {pass_['name']:<30} {pass_['cpu_ms']:9.1f} ms unit cpu")
    return lines


def flatten(node, prefix=""):
    """`path -> wall_ms` for every node, for history rows and charts."""
    path = f"{prefix}/{node['name']}" if prefix else node["name"]
    out = {path: node["wall_ms"]}
    for child in node.get("children", []):
        out.update(flatten(child, path))
    if "unit_cpu_ms" in node:
        out[path + "#cpu"] = node["unit_cpu_ms"]
        for pass_ in node.get("unit_cpu", []):
            out[f"{path}#cpu/{pass_['name']}"] = pass_["cpu_ms"]
    return out


def main():
    parser = argparse.ArgumentParser(description=__doc__,
                                     formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("project", nargs="?", help="project directory to compile")
    parser.add_argument("--bin", default="target/release/rock", help="rock binary")
    parser.add_argument("--runs", type=int, default=3, help="compiles; the fastest is shown")
    parser.add_argument("--record", type=Path, help="print the last record of this stats file instead")
    parser.add_argument("--json", action="store_true", help="print the record instead of the tree")
    args = parser.parse_args()
    if args.record:
        record = json.loads(args.record.read_text().splitlines()[-1])
    elif args.project:
        record, err = compile_budget(args.bin, args.project, args.runs, measurement_env())
        if err:
            sys.exit(err)
    else:
        parser.error("a project directory or --record is required")
    if args.json:
        print(json.dumps(record, indent=1))
        return
    totals = record.get("totals", {})
    print(f"{record['command']} {record['entry']}: {record['wall_ms']:.1f} ms wall, "
          f"{record['threads']} threads, {totals.get('source_files', '?')} files, "
          f"{totals.get('emitted_lua_bytes', 0) / 1e6:.1f} MB emitted, "
          f"peak rss {record.get('peak_rss_bytes', 0) / 1e6:.0f} MB")
    print("\n".join(render(record["budget"], record["wall_ms"])))


if __name__ == "__main__":
    main()
