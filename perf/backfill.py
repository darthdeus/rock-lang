#!/usr/bin/env python3
"""Measure a range of past commits so the charts start with history.

Each commit is checked out into one reused worktree, built into a shared
release target directory (so consecutive commits rebuild incrementally), and
measured with perf/run.py against the corpora of the current checkout, so every
revision compiles the same source (a revision that cannot compile it records an
error). Commits already in the history are skipped unless --force is given.

Usage: perf/backfill.py --since 2026-11-01 [--step 5] [--max 20] [-- run.py args]
       perf/backfill.py --revs sha1 sha2 ...
"""
import argparse
import json
import os
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from run import default_history  # noqa: E402

REPO = Path(__file__).resolve().parent.parent


def git(*args, cwd=REPO):
    return subprocess.run(["git", *args], cwd=cwd, check=True, text=True,
                          capture_output=True).stdout.strip()


def measured_shas(history):
    if not history.exists():
        return set()
    return {json.loads(line)["sha"] for line in history.read_text().splitlines() if line.strip()}


def main():
    parser = argparse.ArgumentParser(description=__doc__,
                                     formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--since", help="first-parent commits on master since this date")
    parser.add_argument("--revs", nargs="*", help="explicit commits instead of --since")
    parser.add_argument("--step", type=int, default=1, help="measure every Nth commit")
    parser.add_argument("--max", type=int, default=0, help="at most this many commits")
    parser.add_argument("--history", type=Path, default=default_history())
    parser.add_argument("--force", action="store_true", help="remeasure commits already in history")
    parser.add_argument("--worktree", type=Path,
                        default=Path(tempfile.gettempdir()) / "rock-perf-worktree")
    parser.add_argument("--target-dir", type=Path,
                        default=Path(tempfile.gettempdir()) / "rock-perf-target")
    parser.add_argument("run_args", nargs="*", help="extra arguments after -- for run.py")
    args = parser.parse_args()

    if args.revs:
        shas = [git("rev-parse", r) for r in args.revs]
    elif args.since:
        shas = git("rev-list", "--first-parent", "--reverse", f"--since={args.since}",
                   "master").splitlines()
    else:
        sys.exit("give --since DATE or --revs SHA...")
    shas = shas[::args.step]
    if args.max:
        shas = shas[-args.max:]
    done = set() if args.force else measured_shas(args.history)
    todo = [s for s in shas if s not in done]
    print(f"{len(todo)} of {len(shas)} commits to measure", file=sys.stderr)
    if not todo:
        return

    if args.worktree.exists():
        git("worktree", "remove", "--force", str(args.worktree))
    git("worktree", "add", "--detach", str(args.worktree), todo[0])
    try:
        for sha in todo:
            git("checkout", "--detach", sha, cwd=args.worktree)
            subject = git("show", "-s", "--format=%s", sha)
            print(f"\n== {sha[:10]} {subject}", file=sys.stderr)
            env = dict(os.environ, CARGO_TARGET_DIR=str(args.target_dir))
            build = subprocess.run(["cargo", "build", "--release", "-p", "rock-cli", "--bin", "rock"],
                                   cwd=args.worktree, env=env)
            if build.returncode:
                print(f"build failed at {sha[:10]}; skipping", file=sys.stderr)
                continue
            subprocess.run([sys.executable, "-I", str(REPO / "perf/run.py"),
                            "--repo", str(args.worktree), "--corpus-root", str(REPO),
                            "--bin", str(args.target_dir / "release/rock"),
                            "--history", str(args.history), *args.run_args])
    finally:
        git("worktree", "remove", "--force", str(args.worktree))
        shutil.rmtree(args.worktree, ignore_errors=True)


if __name__ == "__main__":
    main()
