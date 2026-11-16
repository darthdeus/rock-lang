#!/usr/bin/env bash
# Run every arm even when a suspected VM defect makes one fail or crash.
set -euo pipefail
if [[ $# != 1 ]]; then
    echo "usage: $0 /path/to/luajit" >&2
    exit 2
fi
LJ="$(command -v "$1")"
SOURCE="$(cd "$(dirname "$0")" && pwd)"
SCRATCH="$(mktemp -d)"
trap 'rm -rf "$SCRATCH"' EXIT
case "$(uname -s)" in
    Darwin) EXT=dylib ;;
    Linux) EXT=so ;;
    *) echo "This runner supports Linux and macOS" >&2; exit 2 ;;
esac
"${CC:-cc}" -O1 -shared -fPIC -o "$SCRATCH/libstackargs.$EXT" "$SOURCE/stackargs.c"
echo "LuaJIT: $LJ"
"$LJ" -v
failed=0
run_arm() {
    echo "== $1"
    shift
    local rc=0
    "$@" || rc=$?
    echo "-> exit $rc"
    if (( rc != 0 )); then failed=1; fi
}
run_arm "direct and wrapped10 interpreter" "$LJ" -joff "$SOURCE/run.lua" "$SCRATCH/libstackargs.$EXT"
run_arm "direct and wrapped10 JIT (inspect trace aborts)" "$LJ" -jv "$SOURCE/run.lua" "$SCRATCH/libstackargs.$EXT"
for mode in -joff -jon; do
    run_arm "wrapped15 ($mode)" "$LJ" "$mode" "$SOURCE/crash.lua" "$SCRATCH/libstackargs.$EXT"
done
exit "$failed"
