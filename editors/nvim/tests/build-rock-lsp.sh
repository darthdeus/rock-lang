#!/usr/bin/env bash
# Build (or reuse a cached) `rock` binary for the test container.
# The LSP runs as `rock lsp serve`; there is no separate rock-lsp binary.
#
# The resulting linux binary lives at:
#   ~/.cache/rock-lsp/builds/<arch>/<sha>/rock
#
# `<arch>` matches docker's TARGETARCH (amd64 / arm64). `<sha>` is a
# content hash over crates/ source and the workspace manifest/lockfile.
# The registration owner and compiler dependencies share this identity.
#
# Cargo's target dir is mounted from a persistent host volume
# (~/.cache/rock-lsp/cargo-target/<arch>) so cache misses are
# typically incremental rebuilds (<5s) rather than from-scratch
# (60-90s).
#
# On success, prints the path to the cached binary on stdout. All
# logging goes to stderr so the caller can `BIN=$(./build-rock-lsp.sh)`.
set -euo pipefail

ROCK_DIR=${ROCK_DIR:-$(cd "$(dirname "$0")/../../.." && pwd)}
if [ ! -f "$ROCK_DIR/Cargo.lock" ] || [ ! -d "$ROCK_DIR/crates/rock-dev-cli" ]; then
    echo "build-rock-lsp.sh: \$ROCK_DIR=$ROCK_DIR does not look like a rock workspace" >&2
    echo "  (missing Cargo.lock or crates/rock-dev-cli/)" >&2
    exit 1
fi

case "$(uname -m)" in
    x86_64|amd64) ARCH=amd64 ;;
    aarch64|arm64) ARCH=arm64 ;;
    *) echo "build-rock-lsp.sh: unsupported host arch $(uname -m)" >&2; exit 1 ;;
esac

CACHE_ROOT=${ROCK_LSP_CACHE:-$HOME/.cache/rock-lsp}
TARGET_DIR="$CACHE_ROOT/cargo-target/$ARCH"
BUILDS_ROOT="$CACHE_ROOT/builds/$ARCH"
mkdir -p "$TARGET_DIR" "$BUILDS_ROOT"

# Content hash. Sort for stability; ignore target/ and .git/.
SHA=$(
    find "$ROCK_DIR/crates" "$ROCK_DIR/Cargo.toml" \
        -type f \( -name '*.rs' -o -name 'Cargo.toml' \) 2>/dev/null \
    | LC_ALL=C sort \
    | xargs shasum -a 256 \
    | shasum -a 256 \
    | awk '{print $1}' \
    | head -c 16
)
LOCK_SHA=$(shasum -a 256 "$ROCK_DIR/Cargo.lock" | head -c 16)
SHA="${SHA}-${LOCK_SHA}"

OUT_DIR="$BUILDS_ROOT/$SHA"
OUT_BIN="$OUT_DIR/rock"

if [ -x "$OUT_BIN" ]; then
    echo "build-rock-lsp.sh: cache hit ($SHA)" >&2
    echo "$OUT_BIN"
    exit 0
fi

echo "build-rock-lsp.sh: cache miss ($SHA), building..." >&2

# Build the builder image once. Tag is content-free so it stays stable.
docker build \
    -f "$(dirname "$0")/Dockerfile.builder" \
    -t rock-lsp-builder \
    "$(dirname "$0")" >&2

# Run cargo build. Mount the workspace read-only (we want to be sure
# the build doesn't drop artifacts inside the source tree) and mount
# the target dir read-write from the host cache.
#
# Output the binary into the target dir; we'll copy it to the cache
# slot afterwards. Build only the rock binary to keep the build scoped.
docker run --rm \
    -v "$ROCK_DIR:/workspace:ro" \
    -v "$TARGET_DIR:/target" \
    -e CARGO_TARGET_DIR=/target \
    -w /workspace \
    rock-lsp-builder \
    cargo build -p rock-dev-cli --bin rock >&2

mkdir -p "$OUT_DIR"
# The mounted target dir was /target inside the container, so the
# binary is at $TARGET_DIR/debug/rock on the host.
if [ ! -x "$TARGET_DIR/debug/rock" ]; then
    echo "build-rock-lsp.sh: build succeeded but $TARGET_DIR/debug/rock missing" >&2
    exit 1
fi
cp "$TARGET_DIR/debug/rock" "$OUT_BIN"
chmod +x "$OUT_BIN"

# Update a `latest` symlink for ad-hoc use (`docker run -v
# ~/.cache/rock-lsp/builds/<arch>/latest/rock-lsp:...`).
ln -sfn "$OUT_DIR" "$BUILDS_ROOT/latest"

echo "build-rock-lsp.sh: built $OUT_BIN" >&2
echo "$OUT_BIN"
