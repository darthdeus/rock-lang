"""Checks for isolated source selection and build provenance."""

import hashlib
import json
from pathlib import Path
import subprocess
import tempfile
import unittest


class PrepareTest(unittest.TestCase):
    def test_source_revision_and_benchmark_hashes(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            benchmark = root / "benchmarks/stone_runtime"
            benchmark.mkdir(parents=True)
            benchmark.joinpath("prepare.py").write_bytes(Path(__file__).with_name("prepare.py").read_bytes())
            benchmark.joinpath("stick.toml").write_text('{ package = "stone" }\n')
            benchmark.joinpath("main.rock").write_text("benchmark source\n")
            for name in ("stone", "stone-native"):
                path = root / "packages" / name
                path.mkdir(parents=True)
                (path / "stick.toml").write_text('{ package = "stone-core" }\n')
            source = root / "packages/stone/engine/rendering/pass.rock"
            source.parent.mkdir(parents=True)
            source.write_text("fn scene_return_pass() {}\npub fn run_scene_passes() {}\n")
            (root / "stdlib").mkdir()
            (root / "stdlib/marker").write_text("first\n")
            subprocess.run(["git", "init", "-q"], cwd=root, check=True)
            subprocess.run(["git", "add", "."], cwd=root, check=True)
            subprocess.run(["git", "-c", "user.name=Test", "-c", "user.email=test@example.invalid", "commit", "-qm", "first"], cwd=root, check=True)
            revision = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=root, text=True).strip()
            (root / "stdlib/marker").write_text("second\n")
            subprocess.run(["git", "add", "."], cwd=root, check=True)
            subprocess.run(["git", "-c", "user.name=Test", "-c", "user.email=test@example.invalid", "commit", "-qm", "second"], cwd=root, check=True)
            compiler = root / "compiler"
            compiler.write_text('#!/bin/sh\nprintf "%s" "$(cat "$ROCK_STDLIB_DIR/marker")" > "$6"\n')
            compiler.chmod(0o755)
            out = root / "out"
            subprocess.run(["python3", str(benchmark / "prepare.py"), str(out), "--compiler", str(compiler), "--source-revision", revision, "--target-os", "osx"], cwd=root, check=True)
            metadata = json.loads((out / "build.json").read_text())
            self.assertEqual(metadata["revision"], revision)
            self.assertEqual(metadata["target_os"], "osx")
            self.assertEqual((out / "retained.lua").read_text(), "first")
            self.assertEqual(metadata["benchmark_sources"]["main.rock"], hashlib.sha256(b"benchmark source\n").hexdigest())

    def test_paired_measurement_alternates_candidates(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            runtime = root / "runtime/bin"
            runtime.mkdir(parents=True)
            luajit = runtime / "luajit"
            luajit.write_text('#!/bin/sh\necho MESH_OK\necho VM test\necho "AUDIT mesh $AUDIT_METRIC 1 1 2 3 0 0"\n')
            luajit.chmod(0o755)
            native = root / "native.so"
            native.write_text("native")
            for name in ("baseline", "candidate"):
                artifacts = root / name
                artifacts.mkdir()
                (artifacts / "retained.lua").write_text(name)
                (artifacts / "build.json").write_text(json.dumps({"revision": name}))
            out = root / "out"
            subprocess.run(["python3", str(Path(__file__).with_name("measure.py")), str(root / "baseline"),
                            "--compare", str(root / "candidate"), "--arm", "growth", "--case", "mesh",
                            "--runtime", str(runtime.parent), "--stone-lib", str(native),
                            "--rounds", "2", "--outdir", str(out)], check=True, capture_output=True)
            result = json.loads((out / "results.json").read_text())
            self.assertEqual([sample["variant"] for sample in result["samples"][:4]],
                             ["baseline", "candidate", "candidate", "baseline"])
            self.assertEqual(result["candidate_build"]["revision"], "candidate")

    def test_coalescing_gate_checks_candidate_only_in_paired_runs(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            runtime = root / "runtime/bin"
            runtime.mkdir(parents=True)
            luajit = runtime / "luajit"
            luajit.write_text("""#!/bin/sh
case "$1" in
  */baseline/*) kind=baseline ;;
  *) kind=candidate ;;
esac
echo MESH_OK
echo VM test
echo "AUDIT mesh $AUDIT_METRIC 1 1 2 3 0 0"
if [ "$AUDIT_METRIC" = trace ]; then
  if [ "$kind" = baseline ]; then
    echo 'ABORT 1 warmup:NYI: register coalescing too complex detail=0'
  elif [ -f "$(dirname "$1")/abort" ]; then
    echo 'ABORT 1 measure:NYI: register coalescing too complex detail=0'
  else
    echo 'SITE 1 warmup:NYI: register coalescing too complex detail=0'
  fi
fi
""")
            luajit.chmod(0o755)
            native = root / "native.so"
            native.write_text("native")
            for name in ("baseline", "candidate"):
                artifacts = root / name
                artifacts.mkdir()
                (artifacts / "retained.lua").write_text(name)
                (artifacts / "build.json").write_text(json.dumps({"revision": name}))
            command = ["python3", str(Path(__file__).with_name("measure.py")), str(root / "baseline"),
                       "--compare", str(root / "candidate"), "--arm", "growth", "--case", "mesh",
                       "--runtime", str(runtime.parent), "--stone-lib", str(native), "--rounds", "1",
                       "--outdir", str(root / "out"), "--require-no-coalescing"]
            success = subprocess.run(command, capture_output=True, text=True)
            self.assertEqual(success.returncode, 0, success.stderr)
            (root / "candidate/abort").touch()
            failure = subprocess.run(command, capture_output=True, text=True)
            self.assertNotEqual(failure.returncode, 0)
            self.assertIn("register coalescing too complex", failure.stderr)
            single = [arg for arg in command if arg not in ("--compare", str(root / "candidate"))]
            failure = subprocess.run(single, capture_output=True, text=True)
            self.assertNotEqual(failure.returncode, 0)
            self.assertIn("register coalescing too complex", failure.stderr)


if __name__ == "__main__":
    unittest.main()
