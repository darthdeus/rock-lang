#!/usr/bin/env bash
# Build the test image, build (or reuse a cached) `rock` binary,
# and run tests/run.sh inside the container with the binary mounted
# at /usr/local/bin/rock (the LSP runs as `rock lsp serve`). No host nvim,
# no host toolchain involved; set ROCK_DIR to build a checkout other than
# this repository's root.
#
# Set ROCK_LSP_SKIP=1 to skip the rock build and run only specs
# that don't need a real LSP.
set -euo pipefail

cd "$(dirname "$0")/.."

IMAGE=${IMAGE:-rock-nvim-test}

docker build -t "$IMAGE" .

EXTRA_MOUNTS=()
EXTRA_ENV=()
if [ "${ROCK_LSP_SKIP:-0}" = "1" ]; then
    echo "docker-run.sh: ROCK_LSP_SKIP=1 set; running without a built rock" >&2
else
    BIN=$(tests/build-rock-lsp.sh)
    EXTRA_MOUNTS+=(-v "$BIN:/usr/local/bin/rock:ro")
    EXTRA_ENV+=(-e "ROCK_LSP_BIN=/usr/local/bin/rock")
fi

docker run --rm \
    -v "$(pwd):/plugin:ro" \
    "${EXTRA_MOUNTS[@]}" \
    "${EXTRA_ENV[@]}" \
    "$IMAGE"
