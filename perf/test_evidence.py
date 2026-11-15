import importlib.util
from pathlib import Path
import unittest

spec = importlib.util.spec_from_file_location("check_evidence", Path(__file__).with_name("check_evidence.py"))
check = importlib.util.module_from_spec(spec)
spec.loader.exec_module(check)


class EvidenceTests(unittest.TestCase):
    def test_success_exit_with_zero_matching_tests_is_not_evidence(self):
        item = {"exit": 0, "output": r"test result: ok\. 1 passed;"}
        self.assertFalse(check.matches(item, 0, "test result: ok. 0 passed;"))
        self.assertTrue(check.matches(item, 0, "test result: ok. 1 passed;"))
        self.assertFalse(check.matches(item, 1, "test result: ok. 1 passed;"))

    def test_expected_failure_must_have_the_specific_diagnostic(self):
        item = {"exit": 1, "output": "uint shift count must be in the range"}
        self.assertFalse(check.matches(item, 1, "file not found"))
        self.assertFalse(check.matches(item, 0, item["output"]))
        self.assertTrue(check.matches(item, 1, item["output"]))


if __name__ == "__main__":
    unittest.main()
