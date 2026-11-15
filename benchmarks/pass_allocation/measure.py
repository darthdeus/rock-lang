"""Alternate fresh processes, retaining samples and VM/input identities."""

import argparse
import hashlib
import json
from pathlib import Path
import statistics
import subprocess
import sys

parser = argparse.ArgumentParser()
parser.add_argument("lua", type=Path)
parser.add_argument("--luajit", default="luajit")
parser.add_argument("--rounds", type=int, default=6)
parser.add_argument("--changes", nargs="+", choices=("stable", "changing", "every_frame"),
                    default=("stable", "changing", "every_frame"))
args = parser.parse_args()
driver = Path(__file__).with_name("driver.lua")
rows = []
for inputs in (1, 7, 10):
    for changes in args.changes:
        for metric in ("time", "heap"):
            for pair in range(args.rounds):
                order = ("fresh", "retained") if pair % 2 == 0 else ("retained", "fresh")
                for mode in order:
                    output = subprocess.check_output(
                        [args.luajit, str(driver), str(args.lua.resolve()), mode,
                         str(inputs), metric, changes], text=True)
                    fields = output.strip().split()
                    if fields[:4] != [mode, str(inputs), metric, changes] or len(fields) != 6:
                        raise RuntimeError(f"Unexpected benchmark output: {output}")
                    rows.append(dict(inputs=inputs, changes=changes, metric=metric,
                                     pair=pair, mode=mode, value=float(fields[4]), sink=int(fields[5])))
            selected = [r for r in rows if (r["inputs"], r["changes"], r["metric"]) == (inputs, changes, metric)]
            if len({r["sink"] for r in selected}) != 1:
                raise RuntimeError("Fresh and retained sinks differ")
            for mode in ("fresh", "retained"):
                values = [r["value"] for r in selected if r["mode"] == mode]
                print(inputs, changes, metric, mode, statistics.median(values),
                      min(values), max(values), file=sys.stderr)
print(json.dumps(dict(
    emitted_sha256=hashlib.sha256(args.lua.read_bytes()).hexdigest(),
    vm=subprocess.check_output([args.luajit, "-v"], text=True).strip(),
    samples=rows), indent=2))
