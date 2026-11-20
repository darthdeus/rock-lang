#!/usr/bin/env python3
"""Time cold compiles of the public workloads with one or two rock binaries.

    perf/compare.py target/release/rock
    perf/compare.py base/rock cand/rock --runs 20 --only stone,tests --phases

Every run is a fresh process; the compiler keeps no on-disk compile cache, so
each one is cold. With two binaries the runs interleave (A B, then B A) so
drift on the machine lands on both, and each workload prints the median of
the per-run ratios cand/base with a 95% confidence interval: an interval
that excludes 1.00 is a change, one that straddles it is noise. Peak RSS is
the rock process's own (os.wait4), not its LuaJIT children's.

The `tests` workload is `rock test tests/`: it compiles every corpus file and
also runs it, but the @tests themselves take well under a second in total.
`--phases` adds the top level of one `ROCK_COMPILE_STATS` budget per compile
workload (see perf/budget.py).
"""
import argparse
import math
import os
import statistics
import subprocess
import sys
import tempfile
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import budget as budget_tool  # noqa: E402

REPO = Path(__file__).resolve().parent.parent
COMPILES = {"stone": "packages/stone", "match3": "demos/match3", "testbed": "demos/testbed"}
WORKLOADS = [*COMPILES, "tests"]


def command(rock, workload, out):
    if workload == "tests":
        return [rock, "test", "tests/"]
    return [rock, "compile", COMPILES[workload], "--output", str(out)]


def timed(argv, env):
    """Wall seconds and peak RSS bytes of one process; exits on failure."""
    start = time.perf_counter()
    proc = subprocess.Popen(argv, cwd=REPO, env=env, stdout=subprocess.DEVNULL,
                            stderr=subprocess.PIPE)
    stderr = proc.stderr.read()
    _, status, usage = os.wait4(proc.pid, 0)
    wall = time.perf_counter() - start
    proc.returncode = os.waitstatus_to_exitcode(status)
    if proc.returncode:
        sys.exit(f"{' '.join(argv)} failed ({proc.returncode}):\n{stderr.decode()[-2000:]}")
    # macOS reports bytes; other supported hosts report KiB.
    rss_bytes = usage.ru_maxrss if sys.platform == "darwin" else usage.ru_maxrss * 1024
    return wall, rss_bytes


def quartiles(values):
    q = statistics.quantiles(values, n=4, method="inclusive")
    return q[0], q[2]


def summary(walls):
    low, high = quartiles(walls)
    median = statistics.median(walls)
    return f"{median * 1000:8.0f} ms  iqr {100 * (high - low) / median:4.1f}%"


def paired_ratio(base, cand):
    """Median of the per-run ratios cand/base and a distribution-free 95%
    confidence interval for it: the order statistics a sign test allows."""
    return median_interval([c / b for b, c in zip(base, cand)])


def median_interval(values):
    ordered = sorted(values)
    n = len(ordered)
    k, tail = 0, 0.0
    while True:
        next_tail = tail + math.comb(n, k) / 2 ** n
        if next_tail > 0.025:
            break
        k, tail = k + 1, next_tail
    if k == 0:
        raise ValueError("a 95% interval needs at least 6 paired runs")
    return statistics.median(ordered), ordered[k - 1], ordered[n - k]


def phases(rock, workload, env):
    record, err = budget_tool.compile_budget(rock, str(REPO / COMPILES[workload]), 3, env)
    if err:
        return {"error": err}
    return {c["name"]: c["wall_ms"] for c in record["budget"].get("children", [])}


def main():
    parser = argparse.ArgumentParser(description=__doc__,
                                     formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("bins", nargs="+", metavar="ROCK", help="one binary, or base then candidate")
    parser.add_argument("--runs", type=int, default=10)
    parser.add_argument("--warmup", type=int, default=1, help="untimed runs per binary and workload")
    parser.add_argument("--only", default=",".join(WORKLOADS),
                        help="workloads, comma-separated: " + ", ".join(WORKLOADS))
    parser.add_argument("--phases", action="store_true", help="print each compile's budget top level")
    args = parser.parse_args()
    if len(args.bins) > 2 or args.runs < 6:
        parser.error("give one or two binaries and --runs of at least 6")
    bins = [str(Path(b).resolve()) for b in args.bins]
    workloads = [w for w in args.only.split(",") if w]
    if unknown := set(workloads) - set(WORKLOADS):
        parser.error(f"unknown workload(s): {', '.join(sorted(unknown))}")
    env = budget_tool.measurement_env()
    sys.stdout.reconfigure(line_buffering=True)
    print(f"{os.cpu_count()} cpus, load {os.getloadavg()[0]:.1f}, {args.runs} runs", file=sys.stderr)

    with tempfile.TemporaryDirectory(prefix="rock-compare-") as tmp:
        out = Path(tmp) / "out.lua"
        for workload in workloads:
            walls, rss = [[] for _ in bins], [[] for _ in bins]
            for rock in bins:
                for _ in range(args.warmup):
                    timed(command(rock, workload, out), env)
            for run in range(args.runs):
                order = list(range(len(bins)))[::-1 if run % 2 else 1]
                for i in order:
                    wall, peak = timed(command(bins[i], workload, out), env)
                    walls[i].append(wall)
                    rss[i].append(peak)
            for i, label in enumerate(["base", "cand"][:len(bins)]):
                print(f"{workload:8} {label}  {summary(walls[i])}  "
                      f"rss {statistics.median(rss[i]) / 1e6:5.0f} MB")
            if len(bins) == 2:
                median, low, high = paired_ratio(*walls)
                verdict = "noise" if low <= 1 <= high else ("slower" if median > 1 else "faster")
                print(f"{workload:8} cand/base {median:.3f}  95% ci [{low:.3f}, {high:.3f}]  {verdict}")
            if args.phases and workload in COMPILES:
                tables = [phases(rock, workload, env) for rock in bins]
                for name in tables[0]:
                    cells = "  ".join(f"{t.get(name, float('nan')):8.1f}" for t in tables)
                    print(f"  {name:28} {cells} ms")


if __name__ == "__main__":
    main()
