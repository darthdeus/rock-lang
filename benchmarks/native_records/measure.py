"""Measure table and NativeBuffer records with fresh, alternating LuaJIT processes."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from paired import pairs, spread

CASES = [
    *((work, 4096) for work in ("scan", "prefix", "random", "update", "step", "churn")),
    *((work, n) for work in ("copy", "bulk") for n in (1000, 10000, 100000)),
]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("lua", type=Path)
    parser.add_argument("--luajit", default="luajit")
    parser.add_argument("--rounds", type=int, default=7)
    parser.add_argument("--metric", choices=("time", "heap", "trace", "dump"), required=True)
    parser.add_argument("--only", choices=[f"{work}:{n}" for work, n in CASES], action="append")
    parser.add_argument("--dump-dir", type=Path)
    parser.add_argument("--lock", type=Path, help="exclusive timing lock path")
    args = parser.parse_args()
    if args.rounds < 1:
        parser.error("--rounds must be positive")
    if args.metric == "time" and args.lock is None:
        parser.error("time measurement needs --lock")
    if args.metric == "dump":
        if args.dump_dir is None:
            parser.error("dump measurement needs --dump-dir")
        args.dump_dir.mkdir(parents=True, exist_ok=True)

    driver = Path(__file__).with_name("driver.lua")
    prefix = [args.luajit, str(driver), str(args.lua.resolve())]
    selected = [(work, n) for work, n in CASES if not args.only or f"{work}:{n}" in args.only]
    # Keep verification outside the timing lock and out of every fresh VM.
    for n in sorted({n for _, n in selected}):
        subprocess.run([*prefix, "table", "scan", str(n), "verify", "0"], check=True)

    load_at_start = os.getloadavg()
    samples = []
    rounds = args.rounds if args.metric in ("time", "heap") else 1
    for work, n in selected:
        if args.metric == "time":
            target = {"churn": 1_000_000, "copy": 240_000, "bulk": 240_000}.get(work, 4_000_000)
            reps = max(12, target // n)
        else:
            reps = max(4, 40_000 // n)

        def run(mode):
            command = [*prefix, mode, work, str(n), args.metric, str(reps)]
            if args.metric == "time":
                lock = str(args.lock.resolve())
                command = (["lockf", "-k", lock] if sys.platform == "darwin" else ["flock", lock]) + command
            elif args.metric == "dump":
                command.append(str(args.dump_dir / f"{mode}-{work}-{n}.txt"))
            *events, result = subprocess.check_output(command, text=True).splitlines()
            tag, value, offheap, sink, digest = result.split("\t")
            if tag != "result":
                raise RuntimeError(f"Unexpected benchmark output: {result}")
            return dict(value=float(value), offheap=float(offheap), sink=(sink, digest), events=events)

        cell = pairs(rounds, ("table", "native"), run)
        samples.extend(dict(work=work, rows=n, reps=reps, **sample) for sample in cell)
        for mode in ("table", "native"):
            values = [sample["value"] for sample in cell if sample["mode"] == mode]
            if args.metric in ("time", "heap"):
                print(work, n, mode, *spread(values), file=sys.stderr)

    print(json.dumps(dict(
        metric=args.metric, emitted_sha256=hashlib.sha256(args.lua.read_bytes()).hexdigest(),
        vm=subprocess.check_output([args.luajit, "-v"], text=True).strip(),
        rock_environment={key: value for key, value in sorted(os.environ.items())
                          if key.startswith(("ROCK_JIT_", "ROCK_NB_OFFHEAP_"))},
        load_at_start=load_at_start, load_at_end=os.getloadavg(), samples=samples,
    ), indent=2))


if __name__ == "__main__":
    main()
