"""Inventory boundaries matter more than the presentation of the totals."""
import importlib.util
from pathlib import Path
import subprocess
import tempfile
import unittest

spec = importlib.util.spec_from_file_location("inventory", Path(__file__).with_name("inventory.py"))
inventory = importlib.util.module_from_spec(spec)
spec.loader.exec_module(inventory)


class InventoryTests(unittest.TestCase):
    def test_test_files_and_inline_tests_are_not_conflated(self):
        row = inventory.measure("crates/example/src/lib.rs", "fn f() {}\n#[test]\nfn check() {}\n")
        self.assertEqual(row["role"], "source (may contain inline tests)")
        self.assertEqual(row["test_markers"], 1)
        self.assertEqual(row["lines"], 3)
        row = inventory.measure("crates/example/tests/main.rs", "// helper\n")
        self.assertEqual(row["role"], "test/fixture files")
        self.assertEqual(row["test_markers"], 0)

    def test_corpus_groups_are_locations_not_inferred_features(self):
        row = inventory.measure("tests/values/refs.rock", "@test\nfn writes() {}\n")
        self.assertEqual(row["group"], "tests/values")
        self.assertEqual(row["test_markers"], 1)
        self.assertEqual(inventory.measure("crates/la-arena/src/lib.rs", "")["origin"], "upstream-derived")
        self.assertEqual(inventory.measure("packages/stone/prebuilt/cimgui/wrap.lua", "")["origin"], "prebuilt bindings")

    def test_rejection_cases_need_nonempty_diagnostic_expectations(self):
        source = """//=== TEST CASE: pinned_TypeMismatch ===
//! expect: expected int
fn main() {}
//=== TEST CASE: unpinned_TypeMismatch ===
//! expect:
fn main() {}
"""
        row = inventory.measure("tests/errors.rock.err", source)
        self.assertEqual(row["error_cases"], 2)
        self.assertEqual(row["unpinned_errors"], ["unpinned_TypeMismatch"])

    def test_error_case_stops_at_other_delimiters_like_the_runner(self):
        source = "//=== TEST CASE: wrong_TypeMismatch ===\nfn main() {}\n//=== END ===\n//! expect: outside the case\n"
        row = inventory.measure("tests/errors.rock.err", source)
        self.assertEqual(row["unpinned_errors"], ["wrong_TypeMismatch"])

    def test_inventory_excludes_untracked_and_deleted_files(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            subprocess.run(["git", "init", "-q", tmp], check=True)
            (root / "kept.rs").write_text("fn main() {}\n")
            (root / "deleted.rs").write_text("gone")
            subprocess.run(["git", "-C", tmp, "add", "."], check=True)
            (root / "deleted.rs").unlink()
            (root / "untracked.rs").write_text("not in scope")
            rows, missing = inventory.collect(root)
            self.assertEqual([r["path"] for r in rows], ["kept.rs"])
            self.assertEqual(missing, ["deleted.rs"])

    def test_workspace_selection_and_local_edges(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / "Cargo.toml").write_text('[workspace]\nmembers=["a", "b"]\ndefault-members=["a"]\n')
            for name in ("a", "b"):
                (root / name).mkdir()
                (root / name / "Cargo.toml").write_text(f'[package]\nname="{name}"\nversion="0.1.0"\n')
            with (root / "a/Cargo.toml").open("a") as f:
                f.write('[dependencies]\nb={path="../b"}\n')
            workspace = inventory.workspace(root)
            self.assertEqual(workspace["excluded_from_default"], ["b"])
            self.assertEqual(workspace["local_dependencies"]["a"], ["b"])


if __name__ == "__main__":
    unittest.main()
