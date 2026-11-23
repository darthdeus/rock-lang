#!/usr/bin/env python3
"""Run the corpus with each switchable MIR pass disabled and compare with the default build.

usage: scripts/pass_differential.py [--rock PATH] [--passes a,b] [--corpus DIR]
                                    [--checks] [--jobs N]

Passes are those of ROCK_MIR_DISABLE (docs/compiler.md). A pass whose removal
changes a test result is either a bug in the pass that hid a bug elsewhere, or
a bug the pass was masking in the default build. Failures that the disabled
pass is expected to cause (the @noalloc and nobox guarantees depend on SROA,
copy propagation, inlining and nobox flattening, and SROA keeps functions under
LuaJIT's register limit) are counted apart.
Exit status is 1 when an unexpected difference remains.
"""
import argparse
import concurrent.futures as cf
import os
import re
import subprocess
import sys

PASSES = ["sroa", "copyprop", "foldcmp", "foldbr", "inline", "lambda", "forl", "bounds",
          "noboxflat", "mem2reg", "noboxfold", "latefloat", "callfwd"]
# "too wide" errors (register slots without SROA) name the function as `@/path::fn`.
# Runtime representation checks (no carriers, no allocation, scalar captures)
# also rely on SROA.
EXPECTED = re.compile(r"@noalloc|nobox|boxes `|constructs a tuple table|^`@/"
                      r"|must not materialize carriers|bytes per slot update|not a scalar leaf")
ANSI = re.compile(r"\x1b\[[0-9;]*m")
FILE_FAIL = re.compile(r"^(\S+\.rock)\.* FAILED\s*(.*)$")
TEST_FAIL = re.compile(r"^  (\w+): (?:error: \S+:\d+: )?(.*)$")
ERROR_FAIL = re.compile(r"^\s+(tests/errors/\S+)\s+--\s+(.*)$")
SUMMARY = re.compile(r"rock test: (\d+)/(\d+) @tests passed")


def run(rock, corpus, disabled, checks):
    env = dict(os.environ)
    env.pop("ROCK_MIR_DISABLE", None)
    env["ROCK_MIR_CHECKS"] = "1" if checks else "0"
    if disabled:
        env["ROCK_MIR_DISABLE"] = disabled
    out = subprocess.run([rock, "test", corpus], env=env, capture_output=True, text=True).stdout
    failures, passed, file = {}, None, None
    for line in ANSI.sub("", out).splitlines():
        m = TEST_FAIL.match(line)
        if file and m:
            # A failing @test names its own reason under the file's summary line.
            failures.pop((file, summary), None)
            failures[(file, f"{m.group(1)}: {m.group(2).strip()}")] = True
            continue
        file = None
        for pattern in (FILE_FAIL, ERROR_FAIL):
            m = pattern.match(line)
            if m:
                summary = m.group(2).strip()
                failures[(m.group(1), summary)] = True
                file = m.group(1) if pattern is FILE_FAIL else None
        m = SUMMARY.search(line)
        if m:
            passed = int(m.group(1))
    if passed is None:
        failures[("rock test", "no summary line")] = True
    return set(failures), passed


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("--rock", default=os.environ.get("ROCK_BIN", "rock"))
    ap.add_argument("--passes", default=",".join(PASSES))
    ap.add_argument("--corpus", default="tests/")
    ap.add_argument("--checks", action="store_true", help="also set ROCK_MIR_CHECKS=1")
    ap.add_argument("--jobs", type=int, default=2)
    args = ap.parse_args()
    passes = [p for p in args.passes.split(",") if p]
    base, base_passed = run(args.rock, args.corpus, "", args.checks)
    print(f"default: {len(base)} failing, {base_passed} @tests passed")
    unexpected = 0
    with cf.ThreadPoolExecutor(args.jobs) as pool:
        results = list(pool.map(lambda p: run(args.rock, args.corpus, p, args.checks), passes))
    for name, (failures, passed) in zip(passes, results):
        new = sorted(failures - base)
        bad = [f for f in new if not EXPECTED.search(f[1])]
        print(f"off:{name}: {len(new)} new failures, {len(new) - len(bad)} expected, "
              f"@tests passed {passed} (default {base_passed})")
        for path, reason in bad:
            print(f"  UNEXPECTED {path}: {reason}")
        unexpected += len(bad)
    return 1 if unexpected else 0


if __name__ == "__main__":
    sys.exit(main())
