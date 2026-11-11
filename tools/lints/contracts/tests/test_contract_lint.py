from __future__ import annotations

import contextlib
import io
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import contract_lint  # noqa: E402


class ContractLintTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory()
        self.root = Path(self.temporary.name)
        self.git("init", "-b", "main")
        self.git("config", "user.email", "lint-test@example.invalid")
        self.git("config", "user.name", "Contract lint test")

    def tearDown(self) -> None:
        self.temporary.cleanup()

    def git(self, *args: str) -> str:
        result = subprocess.run(
            ["git", *args], cwd=self.root, text=True, capture_output=True, check=True
        )
        return result.stdout

    def write(self, path: str, content: str) -> None:
        target = self.root / path
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(content, encoding="utf-8")

    def commit(self, message: str) -> None:
        self.git("add", "-A")
        self.git("commit", "-m", message)

    def create_base(self, version: str = "0.1.0") -> None:
        self.write("contracts/storage/VERSION", version + "\n")
        self.write("contracts/storage/query.wit", "package example:storage;\n")
        self.commit("base contract")
        self.git("update-ref", "refs/remotes/origin/main", "HEAD")
        self.git("switch", "-c", "feature")

    def lint(self) -> tuple[int, str]:
        output = io.StringIO()
        with contextlib.redirect_stdout(output), contextlib.redirect_stderr(output):
            result = contract_lint.run(self.root, base="main")
        return result, output.getvalue()

    def test_unchanged_contract_passes(self) -> None:
        self.create_base()
        self.write("README.md", "unrelated change\n")
        self.commit("unrelated")
        result, output = self.lint()
        self.assertEqual(result, 0, output)
        self.assertIn("no public contract changes", output)

    def test_contract_change_without_version_bump_fails(self) -> None:
        self.create_base()
        self.write("contracts/storage/query.wit", "package example:storage;\nworld v2 {}\n")
        self.commit("change contract")
        result, output = self.lint()
        self.assertEqual(result, 1)
        self.assertIn("contract changes require a VERSION change", output)

    def test_version_bump_down_fails(self) -> None:
        self.create_base("0.2.0")
        self.write("contracts/storage/query.wit", "package example:storage;\nworld v2 {}\n")
        self.write("contracts/storage/VERSION", "0.1.9\n")
        self.commit("decrease version")
        result, output = self.lint()
        self.assertEqual(result, 1)
        self.assertIn("version must increase", output)

    def test_patch_bump_passes_without_adr(self) -> None:
        self.create_base()
        self.write("contracts/storage/query.wit", "package example:storage;\nworld v2 {}\n")
        self.write("contracts/storage/VERSION", "0.1.1\n")
        self.commit("patch contract")
        result, output = self.lint()
        self.assertEqual(result, 0, output)

    def test_minor_bump_at_zero_major_requires_adr(self) -> None:
        self.create_base()
        self.write("contracts/storage/query.wit", "package example:storage;\nworld v2 {}\n")
        self.write("contracts/storage/VERSION", "0.2.0\n")
        self.commit("breaking contract without ADR")
        result, output = self.lint()
        self.assertEqual(result, 1)
        self.assertIn("breaking version bump requires", output)

    def test_minor_bump_at_zero_major_passes_with_matching_adr(self) -> None:
        self.create_base()
        self.write("contracts/storage/query.wit", "package example:storage;\nworld v2 {}\n")
        self.write("contracts/storage/VERSION", "0.2.0\n")
        self.write("docs/adr/0001-storage-contract.md", "Versioning contracts/storage\n")
        self.commit("breaking contract with ADR")
        result, output = self.lint()
        self.assertEqual(result, 0, output)

    def test_new_contract_without_version_fails(self) -> None:
        self.create_base()
        self.write("contracts/events/events.wit", "package example:events;\n")
        self.commit("new contract missing version")
        result, output = self.lint()
        self.assertEqual(result, 1)
        self.assertIn("new contract must contain VERSION", output)

    def test_malformed_version_fails(self) -> None:
        self.create_base()
        self.write("contracts/storage/query.wit", "package example:storage;\nworld v2 {}\n")
        self.write("contracts/storage/VERSION", "0.1\n")
        self.commit("malformed version")
        result, output = self.lint()
        self.assertEqual(result, 1)
        self.assertIn("VERSION must be MAJOR.MINOR.PATCH", output)


if __name__ == "__main__":
    unittest.main()
