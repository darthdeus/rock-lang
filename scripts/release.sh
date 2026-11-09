#!/usr/bin/env bash
# Build the host Rock/Stone runtime archive. Honors CARGO_TARGET_DIR.
set -euo pipefail
root=$(cd "$(dirname "$0")/.." && pwd)
target_dir=${CARGO_TARGET_DIR:-$root/target}
python3 "$root/scripts/gamelibs.py" sync
native=$(python3 "$root/scripts/gamelibs.py" path)
cargo build --manifest-path "$root/Cargo.toml" --locked --release -p rock-cli -p stone-rs -p stone-glsl-runner --bin rock --bin stone-glsl-runner --lib
triple=$(rustc -vV | sed -n 's/^host: //p')
name="rock-$triple"
stage="$root/dist/$name"
rm -rf "$stage"
python3 "$root/scripts/package_runtime.py" "$target_dir/release" "$native" "$stage"
tar -czf "$stage.tar.gz" -C "$root/dist" "$name"
ls -l "$stage.tar.gz"
