"""Compile matched real-renderer arms from an isolated copy of the source tree."""

import argparse
import hashlib
import io
import json
import os
from pathlib import Path
import shutil
import subprocess
import tarfile
import tempfile

root = Path(__file__).resolve().parents[2]
parser = argparse.ArgumentParser()
parser.add_argument("outdir", type=Path)
parser.add_argument("--compiler", type=Path, default=root / "target/release/rock")
parser.add_argument("--source-revision", default="HEAD", help="git commit or ref for packages and stdlib")
parser.add_argument("--target-os", default="linux", choices=("linux", "osx", "windows"))
args = parser.parse_args()
compiler = args.compiler.resolve()
outdir = args.outdir.resolve()
outdir.mkdir(parents=True, exist_ok=True)
revision = subprocess.check_output(["git", "rev-parse", "--verify", args.source_revision + "^{commit}"], cwd=root, text=True).strip()
archive = subprocess.check_output(["git", "archive", revision, "packages", "stdlib"], cwd=root)
with tempfile.TemporaryDirectory(prefix="rock-runtime-audit-") as directory:
    tree = Path(directory)
    with tarfile.open(fileobj=io.BytesIO(archive)) as files:
        files.extractall(tree, filter="data")
    package = tree / "benchmarks/stone_runtime"
    shutil.copytree(Path(__file__).parent, package, ignore=shutil.ignore_patterns("__pycache__"))
    benchmark_sources = {str(path.relative_to(package)): hashlib.sha256(path.read_bytes()).hexdigest()
                         for path in sorted(package.rglob("*")) if path.is_file() and (path.suffix == ".rock" or path.name == "stick.toml")}
    manifest = package / "stick.toml"
    manifest.write_text(manifest.read_text().replace('{ package = "stone" }', '{ path = "../../packages/stone" }')
                        .replace('{ package = "stone-core" }', '{ path = "../../packages/stone-core" }'))
    for name in ("stone", "stone-native"):
        path = tree / "packages" / name / "stick.toml"
        path.write_text(path.read_text().replace('{ package = "stone-core" }', '{ path = "../stone-core" }')
                        .replace('{ package = "stone-native" }', '{ path = "../stone-native" }'))
    env = dict(os.environ, ROCK_STDLIB_DIR=str(tree / "stdlib"))
    source = tree / "packages/stone/engine/rendering/pass.rock"
    original = source.read_text()
    start = original.index("fn scene_return_pass(")
    end = original.index("\npub fn run_scene_passes()", start)
    fresh = '''fn scene_return_pass(source: Canvas, output: Canvas) -> Pass {
    return Pass {
        pipeline: BLIT_PIPELINE,
        slot_inputs: [(sampled_canvas(source), TextureFilter::Nearest)],
        output: output,
        blend: BlendMode::Replace,
    }
}
'''
    for arm in ("retained", "fresh"):
        source.write_text(original if arm == "retained" else original[:start] + fresh + original[end:])
        subprocess.run([str(compiler), "compile", str(package), "--target-os", args.target_os,
                        "--output", str(outdir / f"{arm}.lua")], cwd=tree, env=env, check=True)
metadata = dict(revision=revision, benchmark_sources=benchmark_sources, compiler_sha256=hashlib.sha256(compiler.read_bytes()).hexdigest(),
                outputs={arm: hashlib.sha256((outdir / f"{arm}.lua").read_bytes()).hexdigest()
                         for arm in ("retained", "fresh")})
(outdir / "build.json").write_text(json.dumps(metadata, indent=2) + "\n")
