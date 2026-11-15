"""Keep the subsystem map honest about ownership and evidence freshness."""
from pathlib import Path
import tempfile
import unittest

import overview


class OverviewTests(unittest.TestCase):
    def test_directory_boundaries_and_unmapped_files(self):
        units = [{"id": "a", "paths": ["crates/a"]}]
        self.assertEqual(overview.owner("crates/a/src/lib.rs", units), "a")
        self.assertEqual(overview.owner("crates/abc/src/lib.rs", units), "unmapped")

    def test_overlapping_ownership_is_an_error(self):
        units = [{"id": "a", "paths": ["crates/a"]},
                 {"id": "b", "paths": ["crates/a/tests"]}]
        with self.assertRaisesRegex(ValueError, "multiple owners"):
            overview.owner("crates/a/tests/test.rs", units)

    def test_missing_anchor_cannot_be_labelled_reviewed(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / "source.rs").write_text("fn replacement() {}")
            units = [{"id": "a", "paths": ["source.rs"],
                      "sources": [{"path": "source.rs", "anchor": "fn old()"}],
                      "tests": [], "docs": []}]
            with self.assertRaisesRegex(ValueError, "stale anchor"):
                overview.validate(root, units)

    def test_old_or_dirty_results_are_not_current_passes(self):
        result = {"revision": "old", "dirty": False,
                  "results": [{"id": "rule", "passed": True}]}
        self.assertEqual(overview.check_status("rule", result, "new", False), "stale result")
        result["revision"] = "new"
        self.assertEqual(overview.check_status("rule", result, "new", False), "passed")
        self.assertEqual(overview.check_status("rule", result, "new", True), "stale result")
        self.assertEqual(overview.check_status("missing", result, "new", False), "not run")


if __name__ == "__main__":
    unittest.main()
