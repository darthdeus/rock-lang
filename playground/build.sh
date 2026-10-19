#!/usr/bin/env bash
# Build the playground into playground/dist: the compiler as Wasm plus the
# stdlib sources as one JSON object (file name -> text).
#
#   playground/build.sh
#   python3 -m http.server -d playground 8000   # then open http://localhost:8000
#
# Needs the wasm32-unknown-unknown Rust target and node. Honors CARGO_TARGET_DIR.
set -euo pipefail
root=$(cd "$(dirname "$0")/.." && pwd)
target_dir=${CARGO_TARGET_DIR:-$root/target}
dist=$root/playground/dist

cargo build --manifest-path "$root/Cargo.toml" --locked --profile wasm-release \
  --lib --target wasm32-unknown-unknown -p rock-wasm
mkdir -p "$dist"
cp "$target_dir/wasm32-unknown-unknown/wasm-release/rock_wasm.wasm" "$dist/"
node -e '
  const fs = require("fs"), path = require("path");
  const [dir, out] = process.argv.slice(1);
  const files = Object.fromEntries(fs.readdirSync(dir).filter((n) => n.endsWith(".rock")).sort()
    .map((n) => [n, fs.readFileSync(path.join(dir, n), "utf8")]));
  fs.writeFileSync(out, JSON.stringify(files));
' "$root/stdlib" "$dist/stdlib.json"
ls -l "$dist"
