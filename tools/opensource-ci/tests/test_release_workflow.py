"""Tests for opensource/workflows/release.yml (WP-OSS-08.T4): the manual public release workflow."""

import re
import sys
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE.parent))

import workflow_lint  # noqa: E402

REPO = HERE.parents[2]
WORKFLOW = REPO / "opensource" / "workflows" / "release.yml"
SHA_RE = re.compile(r"^[0-9a-f]{40}$")
FORBIDDEN_IN_RUN = re.compile(r"\$\{\{[^}]*\b(?:github\.event\.|inputs\.|github\.head_ref)[^}]*\}\}")

# A copy of the shape of release.yml that runs cargo test in the build job without starting
# PostgreSQL: the lint must refuse it (the rule the brief asks us to keep seeded).
SEEDED_BAD = """\
name: release
on:
  workflow_dispatch:
permissions:
  contents: read
jobs:
  build:
    runs-on: ubuntu-22.04
    timeout-minutes: 30
    steps:
      - uses: actions/checkout@11bd71901bbe5b1630ceea73d27597364c9af683 # v4.2.2
      - run: cargo build --release --locked -p spot-cli
      - run: cargo test --locked
"""


def load():
    import yaml

    text = WORKFLOW.read_text(encoding="utf-8")
    return text, yaml.safe_load(text)


@unittest.skipIf(workflow_lint.yaml is None, "PyYAML is not installed")
class ReleaseWorkflowTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.text, cls.doc = load()
        cls.jobs = cls.doc["jobs"]

    def steps(self, job):
        return [s for s in self.jobs[job].get("steps", []) if isinstance(s, dict)]

    def test_parses_with_three_jobs(self):
        self.assertEqual(set(self.jobs), {"gate", "build", "publish"})

    def test_only_trigger_is_workflow_dispatch_with_two_inputs(self):
        self.assertEqual(workflow_lint.triggers_of(self.doc), {"workflow_dispatch"})
        raw = self.doc.get("on", self.doc.get(True))
        inputs = raw["workflow_dispatch"]["inputs"]
        self.assertEqual(set(inputs), {"tag", "publish"})
        self.assertIs(inputs["tag"]["required"], True)
        self.assertEqual(inputs["tag"]["type"], "string")
        self.assertEqual(inputs["publish"]["type"], "boolean")
        self.assertIs(inputs["publish"]["default"], False)

    def test_permissions_only_publish_writes(self):
        self.assertEqual(self.doc["permissions"], {"contents": "read"})
        for name, job in self.jobs.items():
            perms = job.get("permissions")
            if name == "publish":
                self.assertEqual(perms, {"contents": "write"})
            elif name == "gate":
                self.assertEqual(perms, {"contents": "read", "actions": "read"})
            else:
                self.assertEqual(perms, {"contents": "read"})
        code = "\n".join(l for l in self.text.splitlines() if not l.lstrip().startswith("#"))
        for banned in ("id-token", "attestations", "write-all"):
            self.assertNotIn(banned, code)

    def test_publish_write_has_lint_allow_and_is_conditional(self):
        self.assertRegex(self.text, r"contents:\s*write\s*#\s*lint-allow:\s*contents\s+\S")
        self.assertEqual(self.jobs["publish"]["if"], "inputs.publish")
        self.assertEqual(self.jobs["publish"]["needs"], ["gate", "build"])
        self.assertEqual(self.jobs["build"]["needs"], "gate")

    def test_every_uses_is_a_full_sha(self):
        found = 0
        for job in self.jobs:
            for step in self.steps(job):
                if "uses" in step:
                    found += 1
                    self.assertRegex(step["uses"].partition("@")[2], SHA_RE, step["uses"])
        self.assertGreaterEqual(found, 6)
        for line in self.text.splitlines():
            if "uses:" in line and not line.lstrip().startswith("#"):
                self.assertRegex(line, r"@[0-9a-f]{40} # v\d+\.\d+\.\d+")

    def test_no_expression_of_inputs_or_event_inside_run(self):
        for job in self.jobs:
            for step in self.steps(job):
                run = step.get("run")
                if isinstance(run, str):
                    self.assertIsNone(FORBIDDEN_IN_RUN.search(run), f"{job}: {step.get('name')}")
                    self.assertNotIn("${{", run, f"{job}: {step.get('name')}")

    def test_no_self_hosted_no_secrets_no_extra_triggers(self):
        code = "\n".join(l for l in self.text.splitlines() if not l.lstrip().startswith("#"))
        self.assertNotIn("self-hosted", code)
        self.assertNotIn("SPOT_RUNNER", code)
        self.assertNotIn("secrets.", code)
        self.assertNotIn("environment:", code)
        for event in ("push:", "schedule:", "pull_request", "workflow_run"):
            self.assertNotIn(event, code)

    def test_runners_timeouts_and_concurrency(self):
        self.assertEqual(self.jobs["build"]["runs-on"], "ubuntu-22.04")
        self.assertEqual(self.jobs["gate"]["runs-on"], "ubuntu-latest")
        self.assertEqual(self.jobs["publish"]["runs-on"], "ubuntu-latest")
        for job in self.jobs.values():
            self.assertIsInstance(job["timeout-minutes"], int)
        conc = self.doc["concurrency"]
        self.assertEqual(conc["group"], "release-${{ inputs.tag }}")
        self.assertIs(conc["cancel-in-progress"], False)

    def test_gate_commands(self):
        gate = "\n".join(s.get("run", "") for s in self.steps("gate"))
        self.assertIn("tools/release/tag_gate.py", gate)
        self.assertIn("--workflow ci.yml", gate)
        self.assertIn("check_release.py version --tag \"$TAG\"", gate)
        self.assertIn("check_release.py licence", gate)
        self.assertIn("--require-signed", gate)
        checkout = next(s for s in self.steps("gate") if "actions/checkout" in s.get("uses", ""))
        self.assertEqual(checkout["with"]["fetch-depth"], 0)
        self.assertIs(checkout["with"]["persist-credentials"], False)

    def test_build_commands_and_artefact_names(self):
        build = "\n".join(s.get("run", "") for s in self.steps("build"))
        for needle in (
            "cargo build --release --locked -p spot-cli",
            "tools/packaging/build.py tarball",
            "tools/packaging/build.py deb",
            "tools/packaging/build.py rpm",
            "check.py containerfile",
            "tools/security/sbom.py",
            "check_release.py checksums --dir dist",
            ".tar.gz",
            ".deb",
            ".rpm",
            "SHA256SUMS",
            ".cdx.json",
        ):
            self.assertIn(needle, build)
        self.assertNotIn("cargo test", build)
        self.assertNotIn("run_python_suites", build)
        self.assertIn("D3", self.text)
        upload = next(s for s in self.steps("build") if "upload-artifact" in s.get("uses", ""))
        self.assertEqual(upload["with"]["if-no-files-found"], "error")

    def test_publish_refuses_existing_release_and_notes(self):
        publish = "\n".join(s.get("run", "") for s in self.steps("publish"))
        self.assertIn('gh release view "$TAG"', publish)
        self.assertIn("--verify-tag", publish)
        self.assertIn("--prerelease", publish)
        self.assertIn("-rc.", publish)
        self.assertIn("pre-release, not for production", publish)
        self.assertIn("nothing is signed", publish)
        self.assertIn("docs/releasing.md", publish)
        self.assertNotIn("git tag", publish)
        self.assertNotIn("git push", publish)

    def test_release_workflow_passes_the_lint(self):
        self.assertEqual(workflow_lint.lint_paths([WORKFLOW]), [])
        self.assertEqual(workflow_lint.lint_paths([WORKFLOW.parent]), [])

    def test_unpinned_copy_is_refused(self):
        bad = self.text.replace("@11bd71901bbe5b1630ceea73d27597364c9af683 # v4.2.2", "@v4")
        findings = workflow_lint.lint_text(bad, "bad.yml")
        self.assertTrue(any("[unpinned-action]" in f for f in findings), findings)

    def test_seeded_cargo_test_without_postgres_is_refused(self):
        findings = workflow_lint.lint_text(SEEDED_BAD, "seeded.yml")
        self.assertTrue(any("[postgres-missing]" in f for f in findings), findings)


if __name__ == "__main__":
    unittest.main()
