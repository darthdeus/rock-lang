"""Alternate fresh processes, retaining samples and VM/input identities."""

import argparse
import hashlib
import json
from pathlib import Path
import subprocess
import sys

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from paired import pairs, spread

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
            def run(mode):
                output = subprocess.check_output(
                    [args.luajit, str(driver), str(args.lua.resolve()), mode,
                     str(inputs), metric, changes], text=True)
                fields = output.strip().split()
                if fields[:4] != [mode, str(inputs), metric, changes] or len(fields) != 6:
                    raise RuntimeError(f"Unexpected benchmark output: {output}")
                return dict(value=float(fields[4]), sink=int(fields[5]))

            samples = pairs(args.rounds, ("fresh", "retained"), run)
            rows.extend(dict(inputs=inputs, changes=changes, metric=metric, **sample) for sample in samples)
            for mode in ("fresh", "retained"):
                values = [r["value"] for r in samples if r["mode"] == mode]
                print(inputs, changes, metric, mode, *spread(values), file=sys.stderr)
print(json.dumps(dict(
    emitted_sha256=hashlib.sha256(args.lua.read_bytes()).hexdigest(),
    vm=subprocess.check_output([args.luajit, "-v"], text=True).strip(),
    samples=rows), indent=2))
