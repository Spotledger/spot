"""Tests for tools/opensource-ci/workflow_lint.py: passing files and one seeded-bad file per rule."""

import subprocess
import sys
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE.parent))

import workflow_lint  # noqa: E402

FIXTURES = HERE / "fixtures"
LINT = HERE.parent / "workflow_lint.py"
REPO = HERE.parents[2]

# fixture file stem -> the rule it must trip
BAD = {
    "self-hosted": "self-hosted",
    "pull-request-target": "pull-request-target",
    "pull-request": "pull-request",
    "unpinned-action": "unpinned-action",
    "missing-permissions": "missing-permissions",
    "write-permission": "write-permission",
    "write-permission-scope": "write-permission",
    "missing-timeout": "missing-timeout",
    "script-injection": "script-injection",
    "secrets-trigger": "secrets-trigger",
    "postgres-missing": "postgres-missing",
    "postgres-missing-order": "postgres-missing",
    "postgres-service": "postgres-service",
    "require-pg-off": "require-pg-off",
}


@unittest.skipIf(workflow_lint.yaml is None, "PyYAML is not installed")
class WorkflowLintTests(unittest.TestCase):
    def lint(self, path, **kw):
        return workflow_lint.lint_paths([path], **kw)

    def test_good_files_pass(self):
        for path in sorted((FIXTURES / "good").glob("*.yml")):
            with self.subTest(path.name):
                self.assertEqual(self.lint(path), [])

    def test_each_bad_file_trips_its_rule(self):
        for stem, rule in BAD.items():
            with self.subTest(stem):
                findings = self.lint(FIXTURES / "bad" / f"{stem}.yml")
                self.assertTrue(any(f"[{rule}]" in f for f in findings), findings)

    def test_every_rule_has_a_bad_file(self):
        rules = {r for r in BAD.values()}
        documented = {
            "self-hosted", "pull-request-target", "pull-request", "unpinned-action",
            "missing-permissions", "write-permission", "missing-timeout",
            "script-injection", "secrets-trigger", "postgres-missing",
            "postgres-service", "require-pg-off",
        }
        self.assertEqual(rules, documented)

    def test_allow_unpinned_flag(self):
        path = FIXTURES / "bad" / "unpinned-action.yml"
        self.assertEqual(self.lint(path, allow_unpinned=True), [])

    def test_write_allowed_only_on_the_same_line(self):
        text = (FIXTURES / "good" / "good-write-allowed.yml").read_text()
        moved = text.replace("contents: write # lint-allow: contents publishing the release tag",
                             "# lint-allow: contents publishing the release tag\n  contents: write")
        findings = workflow_lint.lint_text(moved, "moved.yml")
        self.assertTrue(any("[write-permission]" in f for f in findings), findings)

    def test_postgres_opt_out_needs_a_reason(self):
        text = (FIXTURES / "good" / "good-optout.yml").read_text()
        findings = workflow_lint.lint_text(text.replace("no database", ""), "x.yml")
        self.assertEqual(findings, [])
        findings = workflow_lint.lint_text(
            text.replace("# lint-allow: no-postgres pure unit tests of a tool, no database", "# lint-allow: no-postgres"),
            "x.yml",
        )
        self.assertTrue(any("[postgres-missing]" in f for f in findings), findings)

    def test_all_findings_listed_at_once(self):
        text = (FIXTURES / "bad" / "pull-request-target.yml").read_text().replace(
            "permissions:\n  contents: read\n", ""
        )
        findings = workflow_lint.lint_text(text, "x.yml")
        rules = " ".join(findings)
        self.assertIn("[pull-request-target]", rules)
        self.assertIn("[missing-permissions]", rules)

    def test_yaml_parse_error_is_reported(self):
        findings = workflow_lint.lint_text("name: [unclosed\n", "broken.yml")
        self.assertTrue(any("[parse]" in f for f in findings))

    def test_cli_exit_codes(self):
        ok = subprocess.run([sys.executable, str(LINT), str(FIXTURES / "good")], capture_output=True, text=True)
        self.assertEqual(ok.returncode, 0, ok.stdout)
        bad = subprocess.run([sys.executable, str(LINT), str(FIXTURES / "bad")], capture_output=True, text=True)
        self.assertEqual(bad.returncode, 1)
        self.assertIn("finding(s)", bad.stdout)

    def test_shipped_public_workflows_are_clean(self):
        # In the private repository the source is opensource/workflows; in the exported tree the
        # same files sit in .github/workflows.
        shipped = REPO / "opensource" / "workflows"
        if not shipped.is_dir():
            shipped = REPO / ".github" / "workflows"
        self.assertEqual(self.lint(shipped), [])


if __name__ == "__main__":
    unittest.main()
