#!/usr/bin/env python3
"""Measure one compiler revision and append the result to the perf history.

For every corpus (a project directory) the runner records:

- cold `rock compile` and `rock check` wall time (hyperfine, several runs);
- `rock backend-stats --format json`: bone and skull phase means;
- one cold `rock compile` with `ROCK_COMPILE_STATS`: the time budget tree
  (every level sums to its parent; see `perf/budget.py`);
- `rock incremental-stats --format json`: the hot-reload path after a body
  edit (overlay) and after a signature edit (full rebuild);
- `rock lsp-survey --format json`: the language server's update, hover and
  completion latencies per edit kind.

One JSON row per invocation goes to the history file (`--history`, or
`$ROCK_PERF_HISTORY`, default `~/.local/share/rock-lang/perf/history.jsonl`).
`perf/report.py` turns the history into charts. A metric a revision cannot
produce (an older binary without a subcommand) is recorded as null with the
error text, so backfills over old commits still contribute the metrics they
have.

Usage: perf/run.py [--bin PATH | --no-build] [--repo DIR] [--runs N]
                   [--corpus NAME=DIR ...] [--skip-lsp] [--note TEXT]
"""
import argparse
import json
import os
import platform
import shutil
import statistics
import subprocess
import sys
import tempfile
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import budget as budget_tool  # noqa: E402

SCHEMA = 1


def default_corpora(repo):
    """The stone package plus every demo project, named by directory."""
    corpora = {"stone": "packages/stone"}
    for manifest in sorted((repo / "demos").glob("*/stick.toml")):
        corpora[manifest.parent.name] = f"demos/{manifest.parent.name}"
    return corpora


def default_history():
    env = os.environ.get("ROCK_PERF_HISTORY")
    if env:
        return Path(env)
    return Path.home() / ".local/share/rock-lang/perf/history.jsonl"


def measurement_env():
    # Compiler overrides from the shell must not leak into the series.
    env = {k: v for k, v in os.environ.items() if not k.startswith(("ROCK_", "SKULL_", "BONE_"))}
    env.update(ROCK_INVOCATION_LOG="off", ROCK_FRAME_COMPILE_CACHE="0", ROCK_NO_WARNINGS="1")
    return env


def git(repo, *args):
    return subprocess.run(["git", "-C", str(repo), *args], check=True, text=True,
                          capture_output=True).stdout.strip()


def revision(repo):
    sha = git(repo, "rev-parse", "HEAD")
    dirty = bool(git(repo, "status", "--porcelain", "--untracked-files=no"))
    when, subject = git(repo, "show", "-s", "--format=%ct%n%s", sha).split("\n", 1)
    return dict(sha=sha, short=sha[:10], commit_time=int(when), subject=subject, dirty=dirty)


def build(repo):
    subprocess.run(["cargo", "build", "--release", "-p", "rock-cli", "--bin", "rock"],
                   cwd=repo, check=True)
    return repo / "target/release/rock"


def run_json(rock, args, env, cwd, timeout=1800):
    """Run a `--format json` subcommand; return (parsed, error)."""
    proc = subprocess.run([str(rock), *args, "--format", "json"], cwd=cwd, env=env, text=True,
                          capture_output=True, timeout=timeout)
    if proc.returncode:
        tail = (proc.stderr or proc.stdout).strip().splitlines()[-3:]
        return None, f"exit {proc.returncode}: " + " | ".join(tail)
    try:
        return json.loads(proc.stdout), None
    except json.JSONDecodeError as error:
        return None, f"not JSON: {error}"


def hyperfine(command, runs, warmup, env, cwd):
    with tempfile.NamedTemporaryFile(suffix=".json", delete=False) as out:
        export = out.name
    try:
        proc = subprocess.run(["hyperfine", "-N", "--warmup", str(warmup), "--runs", str(runs),
                               "--export-json", export, command],
                              cwd=cwd, env=env, text=True, capture_output=True, timeout=3600)
        if proc.returncode:
            return None, (proc.stderr or proc.stdout).strip().splitlines()[-1:]
        result = json.load(open(export))["results"][0]
        return dict(mean_ms=result["mean"] * 1000, stddev_ms=(result["stddev"] or 0) * 1000,
                    median_ms=result["median"] * 1000, min_ms=result["min"] * 1000,
                    max_ms=result["max"] * 1000, user_ms=result["user"] * 1000,
                    system_ms=result["system"] * 1000, runs=len(result["times"])), None
    finally:
        os.unlink(export)


def phase_table(mapping, keys):
    return {k: mapping.get(k) for k in keys if k in mapping}


def summarize_backend_stats(report):
    bone, skull = report["bone"], report["skull"]
    setup = skull.get("setup_phases", {})
    return dict(
        files=report["files"],
        bone_ok=bone["ok"], skull_ok=skull["ok"],
        parse_ms=bone["parse_ms"],
        semantic_ms=bone["semantic_ms"],
        bone_phases=bone.get("phases", {}),
        workspace_setup_ms=skull["workspace_setup_ms"],
        emit_ms=skull["emit_ms"],
        emit_setup_ms=sum(setup.values()) if setup else None,
        setup_phases=setup,
        emit_phases_elapsed=skull.get("emit_phases", {}),
    )


def summarize_incremental(report):
    by_kind = {}
    for edit in report["edits"]:
        if edit.get("error") or not edit["samples_ms"]:
            continue
        by_kind.setdefault(edit["kind"], []).append(edit)

    def kind(name, reload):
        edits = [e for e in by_kind.get(name, []) if e["reload"] == reload]
        if not edits:
            return None
        return dict(
            edits=len(edits),
            mean_ms=statistics.mean(e["mean_ms"] for e in edits),
            worst_ms=max(e["max_ms"] for e in edits),
            cook_mean_ms=statistics.mean(e["cook_mean_ms"] for e in edits),
            emit_mean_ms=statistics.mean(e["emit_mean_ms"] for e in edits),
            write_mean_ms=statistics.mean(e["write_mean_ms"] for e in edits),
            reinferred_max=max(e["reinferred"] for e in edits),
            bytes_max=max(e["overlay_bytes"] for e in edits),
        )

    return dict(
        init_ms=report["init_ms"],
        main_bytes=report["main_bytes"],
        body=kind("body", "incremental"),
        signature=kind("signature", "full"),
        errors=[dict(name=e["name"], kind=e["kind"], error=e["error"])
                for e in report["edits"] if e.get("error")],
    )


def summarize_lsp(report):
    out = {}
    for kind in ("body", "signature", "broken"):
        rows = [c for c in report["candidates"] if c["kind"] == kind]
        if not rows:
            continue
        out[kind] = dict(
            edits=len(rows),
            update_mean_ms=statistics.mean(r["update_mean_ms"] for r in rows),
            edit_mean_ms=statistics.mean(r["edit_mean_ms"] for r in rows),
            worst_edit_ms=max(r["edit_max_ms"] for r in rows),
            semtok_mean_ms=statistics.mean(r["semtok_mean_ms"] for r in rows),
            inlay_mean_ms=statistics.mean(r["inlay_mean_ms"] for r in rows),
            hover_mean_ms=statistics.mean(r["hover_mean_ms"] for r in rows),
            completion_mean_ms=statistics.mean(r["compl_mean_ms"] for r in rows),
            reinferred_max=max(r["reinferred"] for r in rows),
        )
    out["modules"] = len(report["modules"])
    out["functions"] = len(report["functions"])
    return out


GROUPS = ("cold", "budget", "phases", "incremental", "lsp")


def measure_corpus(rock, repo, corpus_dir, args, env):
    corpus = {}
    warmup = str(min(2, args.iters))
    if "cold" in args.only:
        with tempfile.TemporaryDirectory(prefix="rock-perf-") as tmp:
            out = Path(tmp) / "out.lua"
            corpus["cold_compile"], err = hyperfine(
                f"{rock} compile {corpus_dir} --output {out}", args.runs, args.warmup_runs, env, repo)
            if err:
                corpus["cold_compile_error"] = err
            corpus["cold_check"], err = hyperfine(
                f"{rock} check {corpus_dir}", args.runs, args.warmup_runs, env, repo)
            if err:
                corpus["cold_check_error"] = err

    if "budget" in args.only:
        record, err = budget_tool.compile_budget(str(rock), corpus_dir, min(args.runs, 3), env)
        corpus["budget"] = dict(wall_ms=record["wall_ms"], threads=record["threads"],
                                totals=record.get("totals", {}), tree=record["budget"],
                                flat=budget_tool.flatten(record["budget"])) if record else None
        if err:
            corpus["budget_error"] = err

    if "phases" in args.only:
        report, err = run_json(rock, ["backend-stats", corpus_dir, "--iters", str(args.iters),
                                      "--warmup", warmup], env, repo)
        corpus["backend_stats"] = summarize_backend_stats(report) if report else None
        if err:
            corpus["backend_stats_error"] = err

    if "incremental" in args.only:
        report, err = run_json(rock, ["incremental-stats", corpus_dir, "--iters", str(args.iters),
                                      "--warmup", warmup], env, repo)
        corpus["incremental"] = summarize_incremental(report) if report else None
        if err:
            corpus["incremental_error"] = err

    if "lsp" in args.only and not args.skip_lsp:
        report, err = run_json(rock, ["lsp-survey", corpus_dir, "--iters", str(args.lsp_iters),
                                      "--warmup", "2"], env, repo)
        corpus["lsp"] = summarize_lsp(report) if report else None
        if err:
            corpus["lsp_error"] = err
    return corpus


def headline(corpus):
    """The handful of numbers worth a terminal line."""
    def get(*path):
        node = corpus
        for key in path:
            if not isinstance(node, dict) or node.get(key) is None:
                return None
            node = node[key]
        return node

    return {
        "compile": get("cold_compile", "mean_ms"),
        "check": get("cold_check", "mean_ms"),
        "semantic": get("backend_stats", "semantic_ms"),
        "emit": get("backend_stats", "emit_ms"),
        "setup": get("budget", "flat", "process/codegen/setup"),
        "fan-out": get("budget", "flat", "process/codegen/fan-out"),
        "assembly": get("budget", "flat", "process/codegen/assembly"),
        "body edit": get("incremental", "body", "mean_ms"),
        "sig edit": get("incremental", "signature", "mean_ms"),
        "lsp body": get("lsp", "body", "update_mean_ms"),
        "lsp sig": get("lsp", "signature", "update_mean_ms"),
    }


def previous_row(history_path, sha):
    if not history_path.exists():
        return None
    rows = [json.loads(line) for line in history_path.read_text().splitlines() if line.strip()]
    rows = [r for r in rows if r["sha"] != sha and r.get("status") == "ok"]
    return rows[-1] if rows else None


def print_summary(row, previous):
    machine = row["machine"]
    load = max(machine["load_before"], machine["load_after"])
    if load >= machine["cpus"] * 0.5:
        print(f"warning: load average {load:.1f} on {machine['cpus']} cores; "
              "this row is noisy, rerun on a quiet machine", file=sys.stderr)
    for name, corpus in row["corpora"].items():
        now = headline(corpus)
        before = headline(previous["corpora"][name]) if previous and name in previous["corpora"] else {}
        parts = []
        for key, value in now.items():
            if value is None:
                parts.append(f"{key} n/a")
                continue
            text = f"{key} {value:.1f}ms"
            old = before.get(key)
            if old:
                text += f" ({(value - old) / old * 100:+.0f}%)"
            parts.append(text)
        print(f"{row['short']} {name}: " + ", ".join(parts))
        for key in ("cold_compile_error", "cold_check_error", "backend_stats_error",
                    "incremental_error", "lsp_error"):
            if corpus.get(key):
                print(f"  {key}: {corpus[key]}", file=sys.stderr)


def main():
    parser = argparse.ArgumentParser(description=__doc__,
                                     formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--repo", default=".", help="checkout to measure (git info and corpora)")
    parser.add_argument("--bin", help="rock binary; default builds target/release/rock in --repo")
    parser.add_argument("--no-build", action="store_true",
                        help="use the existing target/release/rock without rebuilding")
    parser.add_argument("--history", type=Path,
                        help="history file; default history.jsonl, or quick.jsonl for --quick, "
                             "under $ROCK_PERF_HISTORY's directory or ~/.local/share/rock-lang/perf")
    parser.add_argument("--corpus", action="append", metavar="NAME=DIR",
                        help="project to measure, relative to --corpus-root (repeatable)")
    parser.add_argument("--corpus-root", help="checkout the corpora are read from; default --repo. "
                        "A backfill passes the current checkout so every revision compiles the same source")
    parser.add_argument("--runs", type=int, default=10, help="hyperfine runs per command")
    parser.add_argument("--warmup-runs", type=int, default=2)
    parser.add_argument("--iters", type=int, default=10,
                        help="timed iterations for backend-stats and incremental-stats")
    parser.add_argument("--lsp-iters", type=int, default=15)
    parser.add_argument("--skip-lsp", action="store_true")
    parser.add_argument("--only", default=",".join(GROUPS),
                        help="metric groups to measure, comma-separated: " + ", ".join(GROUPS))
    parser.add_argument("--quick", action="store_true",
                        help="iterate fast: 3 runs, 3 iters, no LSP survey, the stone corpus only "
                             "(about 10 s with --no-build); rows are tagged quick")
    parser.add_argument("--note", default="")
    parser.add_argument("--dry-run", action="store_true", help="print the row; do not append")
    args = parser.parse_args()

    repo = Path(args.repo).resolve()
    if args.quick:
        args.runs, args.warmup_runs, args.iters, args.skip_lsp = 3, 1, 3, True
        args.corpus = args.corpus or ["stone=packages/stone"]
    if args.history is None:
        args.history = default_history()
        if args.quick:
            args.history = args.history.with_name("quick.jsonl")
    args.only = {g.strip() for g in args.only.split(",")}
    unknown = args.only - set(GROUPS)
    if unknown:
        sys.exit(f"unknown --only group(s): {', '.join(sorted(unknown))}")
    if shutil.which("hyperfine") is None:
        sys.exit("hyperfine is required (brew install hyperfine)")
    corpus_root = Path(args.corpus_root).resolve() if args.corpus_root else repo
    corpora = (dict(c.split("=", 1) for c in args.corpus) if args.corpus
               else default_corpora(corpus_root))
    corpora = {name: str(corpus_root / rel) for name, rel in corpora.items()}
    # A corpus a revision does not have is skipped, so the run still records
    # the corpora it does have.
    for name, path in list(corpora.items()):
        if not Path(path).is_dir():
            print(f"corpus {name}: {path} is not a directory; skipped", file=sys.stderr)
            del corpora[name]
    if not corpora:
        sys.exit("no corpus to measure")

    rev = revision(repo)
    if args.bin:
        rock = Path(args.bin).resolve()
    elif args.no_build:
        rock = repo / "target/release/rock"
    else:
        rock = build(repo)
    if not rock.is_file():
        sys.exit(f"no rock binary at {rock}")

    env = measurement_env()
    started = time.time()
    row = dict(schema=SCHEMA, **rev, measured_at=started, note=args.note, quick=args.quick,
               machine=dict(arch=platform.machine(), system=platform.system(),
                            cpus=os.cpu_count(), load_before=os.getloadavg()[0]),
               settings=dict(runs=args.runs, iters=args.iters, lsp_iters=args.lsp_iters),
               corpora={})
    for name, path in corpora.items():
        print(f"measuring {name} ({path}) at {rev['short']}...", file=sys.stderr)
        row["corpora"][name] = measure_corpus(rock, repo, path, args, env)
    row["machine"]["load_after"] = os.getloadavg()[0]
    row["elapsed_s"] = time.time() - started
    row["status"] = "ok" if all(
        c.get("cold_compile") or "cold" not in args.only
        for c in row["corpora"].values()) else "partial"

    print_summary(row, previous_row(args.history, rev["sha"]))
    if args.dry_run:
        print(json.dumps(row, indent=1))
        return
    args.history.parent.mkdir(parents=True, exist_ok=True)
    with args.history.open("a") as out:
        out.write(json.dumps(row, separators=(",", ":")) + "\n")
    print(f"appended to {args.history}", file=sys.stderr)


if __name__ == "__main__":
    main()
