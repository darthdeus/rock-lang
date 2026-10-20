#!/usr/bin/env bash
# Headless test runner for rock.nvim.
#
# Each test is a Lua file under tests/ that loads the plugin from this
# directory's lua/ via package.path, exercises a scenario, and exits with
# os.exit(0) on success or os.exit(1) on failure. Stdout is captured and
# echoed only on failure.
set -euo pipefail

cd "$(dirname "$0")/.."

PASS=0
FAIL=0
FAILED_TESTS=()

for test in tests/*_spec.lua; do
  name=$(basename "$test" .lua)
  out=$(mktemp)
  if nvim --headless --clean \
      --cmd "set rtp+=$(pwd)" \
      -l "$test" >"$out" 2>&1; then
    echo "PASS  $name"
    PASS=$((PASS + 1))
  else
    echo "FAIL  $name"
    cat "$out"
    FAIL=$((FAIL + 1))
    FAILED_TESTS+=("$name")
  fi
  rm -f "$out"
done

echo
echo "=== $PASS passed, $FAIL failed ==="
[ "$FAIL" -eq 0 ]
