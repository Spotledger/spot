"""WP-OSS-07.T6: the public workflows report on same-repository pull requests, safely.

The three shipped workflows (ci, hygiene, ui) trigger on ``pull_request``; the lint accepts that
only with a read-only token, no secret, GitHub-hosted runners, the same-repository guard on every
job and no checkout of another repository. Every case below is a mutation of a shipped file.
"""

from __future__ import annotations

import re
import sys
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE.parent))

import workflow_lint  # noqa: E402

REPO = HERE.parents[2]
# The shipped files live in opensource/workflows in the private repository and in .github/workflows
# in the exported tree (the same files).
WORKFLOWS = REPO / "opensource" / "workflows"
if not WORKFLOWS.is_dir():
    WORKFLOWS = REPO / ".github" / "workflows"
OPS_WORKFLOWS = REPO / "opensource" / "ops" / "workflows"
GUARD = workflow_lint.PR_GUARD
SHIPPED = ("ci.yml", "hygiene.yml", "ui.yml")
CHECKOUT = "actions/checkout@11bd71901bbe5b1630ceea73d27597364c9af683 # v4.2.2"


def read(name: str) -> str:
    return (WORKFLOWS / name).read_text(encoding="utf-8")


def swap(text: str, old: str, new: str, count: int = 1) -> str:
    assert old in text, f"mutation anchor missing: {old!r}"
    return text.replace(old, new, count)


def drop_guard(text: str, job: str) -> str:
    """Removes the guard line of one job (the line right after ``  <job>:``)."""
    return swap(text, f"\n  {job}:\n    if: {GUARD}\n", f"\n  {job}:\n")


def findings(text: str, name: str = "ci.yml") -> list[str]:
    return workflow_lint.lint_text(text, name)


def pr_findings(text: str, name: str = "ci.yml") -> list[str]:
    return [f for f in findings(text, name) if "[pull-request]" in f]


@unittest.skipIf(workflow_lint.yaml is None, "PyYAML is not installed")
class ShippedFilesTests(unittest.TestCase):
    def test_the_three_shipped_files_pass_the_lint(self) -> None:
        for name in SHIPPED:
            with self.subTest(name):
                self.assertEqual(workflow_lint.lint_paths([WORKFLOWS / name]), [])

    def test_triggers_keep_push_schedule_and_dispatch_and_add_pull_request(self) -> None:
        import yaml

        for name in SHIPPED:
            doc = yaml.safe_load(read(name))
            triggers = workflow_lint.triggers_of(doc)
            with self.subTest(name):
                self.assertIn("pull_request", triggers)
                self.assertIn("push", triggers)
                self.assertNotIn("pull_request_target", triggers)
                self.assertEqual(doc[True]["pull_request"], {"branches": ["main"]})
        self.assertIn("schedule", workflow_lint.triggers_of(yaml.safe_load(read("ci.yml"))))
        self.assertIn("schedule", workflow_lint.triggers_of(yaml.safe_load(read("hygiene.yml"))))
        ui = yaml.safe_load(read("ui.yml"))
        self.assertEqual(set(ui[True]["workflow_dispatch"]["inputs"]), {"generate_baselines", "os", "browser_visual"})

    def test_job_and_workflow_names_are_unchanged(self) -> None:
        import yaml

        names = {
            "ci.yml": ("ci", {"python": "python suites, licence lint, workflow lint (${{ matrix.os }})",
                              "rust": "rust (${{ matrix.os }}, shard ${{ matrix.shard }}/4)",
                              "deny": "cargo deny", "gates": "lint gates",
                              "docs": "contract documentation site", "dotnet": "dotnet (${{ matrix.os }})"}),
            "hygiene.yml": ("hygiene", {"scan": "hygiene scan and seeded recall"}),
            "ui.yml": ("ui", {"ui": "UI budgets and pixels (${{ matrix.os }})",
                              "browser-visual": "browser visual baselines (linux, opt-in)"}),
        }
        for file, (workflow, jobs) in names.items():
            doc = yaml.safe_load(read(file))
            with self.subTest(file):
                self.assertEqual(doc["name"], workflow)
                self.assertEqual({k: v["name"] for k, v in doc["jobs"].items()}, jobs)

    def test_every_job_has_the_guard(self) -> None:
        import yaml

        for name in SHIPPED:
            for job_id, job in yaml.safe_load(read(name))["jobs"].items():
                with self.subTest(f"{name}:{job_id}"):
                    self.assertTrue(workflow_lint.has_pr_guard(job.get("if")), job.get("if"))

    def test_a_pull_request_runs_linux_only_like_a_push(self) -> None:
        for name in ("ci.yml",):
            text = read(name)
            self.assertNotIn("github.event_name == 'push' && '[\"ubuntu-latest\"]'", text)
            self.assertEqual(
                text.count("(github.event_name == 'push' || github.event_name == 'pull_request') && '[\"ubuntu-latest\"]'"), 3
            )

    def test_the_opt_in_ui_job_keeps_its_condition_inside_the_guard(self) -> None:
        import yaml

        job = yaml.safe_load(read("ui.yml"))["jobs"]["browser-visual"]
        self.assertIn("github.event.inputs.browser_visual == 'true'", job["if"])
        self.assertTrue(job["if"].startswith(f"({GUARD}) && ("))


@unittest.skipIf(workflow_lint.yaml is None, "PyYAML is not installed")
class MutationTests(unittest.TestCase):
    """Each mutation of a shipped file must trip the lint with a named sub-rule."""

    def assert_trips(self, text: str, needle: str, name: str = "ci.yml") -> None:
        found = pr_findings(text, name)
        self.assertTrue(any(needle in f for f in found), found)

    def test_a_write_scope_fails_even_with_a_lint_allow(self) -> None:
        text = read("ci.yml")
        self.assert_trips(swap(text, "permissions:\n  contents: read\nconcurrency", "permissions:\n  contents: write\nconcurrency"), "permissions: write scope `contents`")
        allowed = swap(text, "permissions:\n  contents: read\nconcurrency", "permissions:\n  contents: write # lint-allow: contents publishing\nconcurrency")
        self.assertEqual([f for f in findings(allowed) if "[write-permission]" in f], [])  # the base rule alone would accept it
        self.assert_trips(allowed, "permissions: write scope `contents`")

    def test_a_job_level_write_scope_fails(self) -> None:
        text = swap(read("hygiene.yml"), "    runs-on: ubuntu-latest\n", "    runs-on: ubuntu-latest\n    permissions:\n      pull-requests: write # lint-allow: pull-requests comment\n")
        self.assert_trips(text, "write scope `pull-requests`", "hygiene.yml")

    def test_top_level_permissions_must_be_exactly_contents_read(self) -> None:
        text = swap(read("ci.yml"), "permissions:\n  contents: read\nconcurrency", "permissions:\n  contents: read\n  actions: read\nconcurrency")
        self.assert_trips(text, "must be exactly `contents: read`")
        self.assert_trips(swap(read("ci.yml"), "permissions:\n  contents: read\n", ""), "must be exactly `contents: read`")

    def test_a_secret_fails_but_the_github_token_does_not(self) -> None:
        text = read("ci.yml")
        with_secret = swap(text, "env:\n  CARGO_TERM_COLOR: always\n", "env:\n  CARGO_TERM_COLOR: always\n  EXTRA: ${{ secrets.DEPLOY_KEY }}\n")
        self.assert_trips(with_secret, "`secrets.DEPLOY_KEY`")
        self.assertTrue(any("[secrets-trigger]" in f for f in findings(with_secret)))
        with_token = swap(text, "env:\n  CARGO_TERM_COLOR: always\n", "env:\n  CARGO_TERM_COLOR: always\n  GH_TOKEN: ${{ secrets.GITHUB_TOKEN }}\n")
        self.assertEqual(pr_findings(with_token), [])
        self.assertEqual([f for f in findings(with_token) if "[secrets-trigger]" in f], [])
        self.assert_trips(swap(text, "jobs:\n", "jobs:\n  reuse:\n    if: " + GUARD + "\n    uses: ./.github/workflows/x.yml\n    secrets: inherit\n"), "secrets: inherit")

    def test_a_job_without_the_guard_fails_and_names_the_job(self) -> None:
        for name, job in (("ci.yml", "rust"), ("ci.yml", "dotnet"), ("hygiene.yml", "scan"), ("ui.yml", "ui")):
            with self.subTest(f"{name}:{job}"):
                self.assert_trips(drop_guard(read(name), job), f"guard: job `{job}` lacks", name)

    def test_a_guard_that_can_be_bypassed_fails(self) -> None:
        text = read("ci.yml")
        bypass = swap(text, f"    if: {GUARD}\n    # lint-allow: no-postgres cargo deny", f"    if: {GUARD} || true\n    # lint-allow: no-postgres cargo deny")
        self.assert_trips(bypass, "guard: job `deny`")
        or_rest = swap(text, f"    if: {GUARD}\n    # lint-allow: no-postgres cargo deny", f"    if: ({GUARD}) && github.run_id > 0 || true\n    # lint-allow: no-postgres cargo deny")
        self.assert_trips(or_rest, "guard: job `deny`")
        step_level = swap(drop_guard(text, "deny"), "    name: cargo deny\n", f"    name: cargo deny\n    steps:\n      - if: {GUARD}\n        run: echo x\n")
        self.assertTrue(any("guard: job `deny`" in f for f in pr_findings(step_level)))

    def test_an_accepted_guard_form_is_accepted(self) -> None:
        text = read("ci.yml")
        wrapped = swap(text, f"    if: {GUARD}\n    # lint-allow: no-postgres cargo deny", f"    if: ({GUARD}) && (github.run_id > 0 || false)\n    # lint-allow: no-postgres cargo deny")
        self.assertEqual(pr_findings(wrapped), [])
        expression = swap(text, f"    if: {GUARD}\n    # lint-allow: no-postgres cargo deny", f"    if: ${{{{ {GUARD} }}}}\n    # lint-allow: no-postgres cargo deny")
        self.assertEqual(pr_findings(expression), [])

    def test_pull_request_target_fails(self) -> None:
        text = swap(read("ci.yml"), "  pull_request:\n", "  pull_request_target:\n")
        self.assertTrue(any("[pull-request-target]" in f for f in findings(text)), findings(text))

    def test_workflow_run_fails(self) -> None:
        text = swap(read("hygiene.yml"), "  pull_request:\n", "  workflow_run:\n    workflows: [ci]\n  pull_request:\n")
        self.assert_trips(text, "workflow-run:", "hygiene.yml")

    def test_a_checkout_of_another_repository_fails(self) -> None:
        text = swap(
            read("hygiene.yml"),
            "    steps:\n",
            f"    steps:\n      - uses: {CHECKOUT}\n        with: {{ repository: someone/else }}\n",
        )
        self.assert_trips(text, "checkout: job `scan`, step 1 checks out another repository", "hygiene.yml")
        fork_head = swap(
            read("hygiene.yml"),
            "    steps:\n",
            f"    steps:\n      - uses: {CHECKOUT}\n        with: {{ ref: '${{{{ github.event.pull_request.head.repo.full_name }}}}' }}\n",
        )
        self.assert_trips(fork_head, "checkout: job `scan`, step 1 checks out a ref of another repository", "hygiene.yml")
        same = swap(
            read("hygiene.yml"),
            "    steps:\n",
            f"    steps:\n      - uses: {CHECKOUT}\n        with: {{ repository: '${{{{ github.repository }}}}' }}\n",
        )
        self.assertEqual(pr_findings(same, "hygiene.yml"), [])

    def test_a_self_hosted_runner_fails(self) -> None:
        text = swap(read("hygiene.yml"), "    runs-on: ubuntu-latest\n", "    runs-on: [self-" "hosted, linux]\n")
        self.assertTrue(any("[self-hosted]" in f for f in findings(text, "hygiene.yml")))

    def test_the_fixture_pair(self) -> None:
        good = (HERE / "fixtures" / "good" / "good-pull-request.yml").read_text()
        self.assertEqual(findings(good, "good.yml"), [])


class GuardParserTests(unittest.TestCase):
    def test_has_pr_guard(self) -> None:
        ok = workflow_lint.has_pr_guard
        self.assertTrue(ok(GUARD))
        self.assertTrue(ok(f"${{{{ {GUARD} }}}}"))
        self.assertTrue(ok(f"({GUARD}) && (a || b)"))
        self.assertFalse(ok(None))
        self.assertFalse(ok(f"{GUARD} && a"))
        self.assertFalse(ok(f"({GUARD}) && a || b"))
        self.assertFalse(ok(f"({GUARD}) && (a) || (b)"))
        self.assertFalse(ok(f"({GUARD}) || true"))
        self.assertFalse(ok("github.event.pull_request.head.repo.full_name == github.repository"))
        self.assertFalse(ok("true"))

    def test_is_wrapped(self) -> None:
        self.assertTrue(workflow_lint.is_wrapped("(a)"))
        self.assertTrue(workflow_lint.is_wrapped("(a && (b))"))
        self.assertTrue(workflow_lint.is_wrapped("(a == ')')"))
        self.assertFalse(workflow_lint.is_wrapped("(a) || (b)"))
        self.assertFalse(workflow_lint.is_wrapped("a"))


class EventPinTests(unittest.TestCase):
    """Only the public ops workflows may react to a comment or another workflow."""

    def triggers(self, path: Path) -> set[str]:
        text = path.read_text(encoding="utf-8")
        return set(re.findall(r"^\s{2}(issue_comment|workflow_run)\s*:", text, re.MULTILINE)) | set(
            re.findall(r"^on:\s*\[?.*\b(issue_comment|workflow_run)\b", text, re.MULTILINE)
        )

    def test_no_public_workflow_uses_issue_comment_or_workflow_run(self) -> None:
        for path in sorted(WORKFLOWS.glob("*.yml")):
            with self.subTest(path.name):
                self.assertEqual(self.triggers(path), set())

    @unittest.skipUnless(OPS_WORKFLOWS.is_dir(), "opensource/ops is private-only: it is not part of the public export")
    def test_in_the_ops_directory_only_the_verdict_workflow_and_the_private_commands_use_issue_comment(self) -> None:
        users = {p.name for p in OPS_WORKFLOWS.glob("*.yml") if "issue_comment" in self.triggers(p)}
        self.assertEqual(users, {"spot-verdict.yml", "ops-commands.yml"})

    def test_no_workflow_anywhere_uses_workflow_run(self) -> None:
        for directory in (WORKFLOWS, OPS_WORKFLOWS):
            for path in sorted(directory.glob("*.yml")):
                with self.subTest(path.name):
                    self.assertNotIn("workflow_run", self.triggers(path))


if __name__ == "__main__":
    unittest.main()
