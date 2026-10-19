#!/usr/bin/env bash
# Build the release archive: a --release `rock` binary plus the stdlib and
# docs, laid out as bin/rock beside stdlib/ so the binary resolves its
# runtime sources without a checkout.
#
#   scripts/release.sh
#
# Writes dist/rock-<target-triple>/ and dist/rock-<target-triple>.tar.gz.
# Honors CARGO_TARGET_DIR.
set -euo pipefail
root=$(cd "$(dirname "$0")/.." && pwd)
target_dir=${CARGO_TARGET_DIR:-$root/target}

cargo build --manifest-path "$root/Cargo.toml" --locked --release -p rock-cli --bin rock
triple=$(rustc -vV | sed -n 's/^host: //p')

name="rock-$triple"
stage="$root/dist/$name"
rm -rf "$stage"
mkdir -p "$stage/bin"
cp "$target_dir/release/rock" "$stage/bin/rock"
cp -r "$root/stdlib" "$stage/stdlib"
cp "$root/LICENSE-MIT" "$root/LICENSE-APACHE" "$root/README.md" "$stage/"
tar -czf "$stage.tar.gz" -C "$root/dist" "$name"
ls -l "$stage.tar.gz"
