#!/usr/bin/env python3
"""Run native tests against the Stone cdylib from the same Cargo build."""
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile


def main():
    root = Path(__file__).resolve().parent.parent
    packages = ["stone-rs", "stone-gl", "stone-capture", "stone-imgui-abi"]
    with tempfile.TemporaryDirectory(prefix="stone-native-tests-") as scratch:
        metadata = Path(scratch) / "binaries.json"
        with metadata.open("w") as output:
            subprocess.run(
                ["cargo", "nextest", "list", "--locked", "--list-type", "binaries-only",
                 "--message-format", "json", *[arg for package in packages for arg in ("-p", package)]],
                cwd=root, stdout=output, check=True,
            )
        binaries = json.loads(metadata.read_text())
        build = binaries["rust-build-meta"]
        package_id = binaries["rust-binaries"]["stone-rs"]["package-id"]
        filename = {"linux": "libstone_rs.so", "darwin": "libstone_rs.dylib",
                    "win32": "stone_rs.dll"}[sys.platform]
        artifacts = {
            Path(build["target-directory"]) / binary["path"]
            for binary in build["non-test-binaries"].get(package_id, [])
            if binary["kind"] == "dylib" and Path(binary["path"]).name == filename
        }
        if len(artifacts) != 1:
            raise SystemExit(f"expected one current Stone cdylib in nextest metadata, found {artifacts}")
        artifact = artifacts.pop()
        if not artifact.is_file():
            raise SystemExit(f"Stone cdylib from nextest metadata is missing: {artifact}")
        print(f"Stone test cdylib: {artifact}", flush=True)
        environment = dict(os.environ, STONE_TEST_LIB=str(artifact))
        subprocess.run(
            ["cargo", "nextest", "run", "--binaries-metadata", str(metadata), "--no-fail-fast"],
            cwd=root, env=environment, check=True,
        )


if __name__ == "__main__":
    try:
        main()
    except subprocess.CalledProcessError as error:
        sys.exit(error.returncode)
