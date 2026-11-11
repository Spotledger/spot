import shutil
import subprocess
import sys
import unittest
from pathlib import Path


LINT = Path(__file__).resolve().parents[1] / "deps_lint.py"
FIXTURES = Path(__file__).resolve().parent / "fixtures"


@unittest.skipUnless(shutil.which("cargo"), "cargo is not installed; skipping fixture metadata tests")
class DependencyLintTests(unittest.TestCase):
    def check_fixture(self, name):
        return subprocess.run(
            [sys.executable, str(LINT), str(FIXTURES / name)],
            capture_output=True,
            text=True,
            check=False,
        )

    def test_clean_workspace(self):
        result = self.check_fixture("clean")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(result.stdout, "")

    def test_framework_normal_dependency_is_rejected(self):
        result = self.check_fixture("framework-normal")
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        self.assertIn("G4: app-one depends on framework-core (normal)", result.stdout)

    def test_framework_dev_dependency_is_rejected(self):
        result = self.check_fixture("framework-dev")
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        self.assertIn("G4: app-one depends on framework-core (dev)", result.stdout)

    def test_allowed_api_and_app_dependency_are_clean(self):
        result = self.check_fixture("allowed")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(result.stdout, "")


if __name__ == "__main__":
    unittest.main()
