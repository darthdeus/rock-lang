"""Rerun the bounded checks in docs/evidence.json; do not infer correctness from anchors."""
import json
import os
from pathlib import Path
import re
import subprocess
import time


def matches(check, returncode, output):
    return returncode == check["exit"] and re.search(check["output"], output) is not None


def main():
    root = Path(__file__).resolve().parent.parent
    ledger = json.loads((root / "docs/evidence.json").read_text())
    destination = root / "target/inventory/evidence"
    destination.mkdir(parents=True, exist_ok=True)
    results = []
    env = dict(os.environ, CLICOLOR="0", NO_COLOR="1", ROCK_INVOCATION_LOG="off")
    for name in ("FORCE_COLOR", "CLICOLOR_FORCE", "ROCK_FORCE_COLOR", "ROCK_LOG_COLOR"):
        env.pop(name, None)
    for claim in ledger:
        stale = [ref["path"] for ref in claim["sources"]
                 if not (root / ref["path"]).is_file()
                 or ref["anchor"] not in (root / ref["path"]).read_text()]
        row = {"id": claim["id"], "stale_sources": stale, "checks": []}
        for index, check in enumerate(claim["checks"]):
            start = time.monotonic()
            try:
                process = subprocess.run(check["command"], cwd=root, env=env, text=True,
                                         stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                                         timeout=600)
                output, returncode = process.stdout, process.returncode
            except (OSError, subprocess.TimeoutExpired) as error:
                output, returncode = str(error), None
            passed = matches(check, returncode, output)
            log = f'{claim["id"]}-{index}.txt'
            (destination / log).write_text(output)
            row["checks"].append({"command": check["command"], "passed": passed,
                                  "exit": returncode, "seconds": round(time.monotonic() - start, 3),
                                  "log": log})
        row["passed"] = not stale and bool(row["checks"]) and all(c["passed"] for c in row["checks"])
        results.append(row)
        print(f'{claim["id"]}: {"PASS" if row["passed"] else "FAIL"}', flush=True)
    summary = {"revision": subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=root, text=True).strip(),
               "dirty": bool(subprocess.check_output(["git", "status", "--porcelain"], cwd=root, text=True).strip()),
               "results": results}
    (destination / "results.json").write_text(json.dumps(summary, indent=2) + "\n")
    raise SystemExit(0 if results and all(row["passed"] for row in results) else 1)


if __name__ == "__main__":
    main()
