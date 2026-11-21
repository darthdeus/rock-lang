#!/usr/bin/env bash
set -euo pipefail

vm=${ROCK_CI_REAL_LUAJIT:?missing real LuaJIT path}
program=
for arg in "$@"; do
    if [[ -f "$arg" ]] && LC_ALL=C grep -Fq 'churned_large_storage_stays_bounded' "$arg"; then
        program=$arg
        break
    fi
done
if [[ -z "$program" ]]; then
    exec "$vm" "$@"
fi

mkdir -p "${ROCK_CI_CAPTURE_DIR:?missing capture directory}"
capture=$(mktemp -d "$ROCK_CI_CAPTURE_DIR/launch.XXXXXX")
cp -- "$program" "$capture/program.lua"
python3 - "$vm" "$program" "$capture" "$@" <<'PYTHON'
import hashlib
import json
import os
from pathlib import Path
import sys

vm, program, capture, *args = sys.argv[1:]
env_keys = (
    "LUA_PATH", "LUA_CPATH", "LUA_INIT", "LD_LIBRARY_PATH", "LD_PRELOAD", "PATH",
    "ROCK_DIR", "ROCK_TEST_TMP", "ROCK_TEST_SHARD_TESTS",
    "ROCK_TEST_GC_COLLECT_MB", "ROCK_TEST_SHARDS", "ROCK_NB_OFFHEAP_MIN", "STONE_HEADLESS", "STONE_JV",
    "MIMALLOC_ALLOW_THP", "MALLOC_ARENA_MAX", "GLIBC_TUNABLES",
    "LANG", "LC_ALL", "LC_NUMERIC",
)
metadata = {
    "vm": str(Path(vm).resolve()),
    "vm_sha256": hashlib.sha256(Path(vm).read_bytes()).hexdigest(),
    "program": program,
    "program_sha256": hashlib.sha256(Path(capture, "program.lua").read_bytes()).hexdigest(),
    "github_sha": os.environ.get("GITHUB_SHA"),
    "argv": [vm, *args],
    "cwd": os.getcwd(),
    "pid": os.getppid(),
    "environment": {key: os.environ.get(key) for key in env_keys},
}
Path(capture, "launch.json").write_text(json.dumps(metadata, indent=2) + "\n")
PYTHON

# exec preserves the PID sampled/killed by the test watchdog. The relays drain
# to EOF even when it kills LuaJIT, and forward the original protocol streams.
exec "$vm" "$@" > >(tee "$capture/stdout") 2> >(tee "$capture/stderr" >&2)
