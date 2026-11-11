"""Tests for tools/ci/run_python_suites.py (WP-PIP-03.T1).

Every behaviour the CI runner must have is checked against temporary directory
trees, so the tests stay independent of the real repository layout: discovery
rules, per-suite subprocesses, timeouts, empty-discovery errors and the
skip.toml rules (reason required, stale entry is an error, OS matching).
"""
from __future__ import annotations

import contextlib
import io
import os
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE.parent))

import run_python_suites  # noqa: E402

PASSING_SUITE = "import unittest\n\n\nclass OK(unittest.TestCase):\n    def test_ok(self):\n        self.assertTrue(True)\n"

FAILING_SUITE = (
    "import unittest\n\n\nclass Bad(unittest.TestCase):\n"
    "    def test_bad(self):\n        self.fail('boom')\n"
)

SLOW_SUITE = (
    "import time\nimport unittest\n\n\nclass Slow(unittest.TestCase):\n"
    "    def test_slow(self):\n        time.sleep(120)\n"
)

STEP_SUMMARY_HEADER = "### Python test suites"


def write(path: Path, text: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(text, encoding="utf-8")


@contextlib.contextmanager
def neutral_step_summary():
    """Run with GITHUB_STEP_SUMMARY unset, restoring the environment after."""
    saved = os.environ.pop("GITHUB_STEP_SUMMARY", None)
    try:
        yield
    finally:
        if saved is not None:
            os.environ["GITHUB_STEP_SUMMARY"] = saved


@contextlib.contextmanager
def captured():
    """Capture the runner's stdout and stderr."""
    out, err = io.StringIO(), io.StringIO()
    with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
        yield out, err


class TempTreeTestCase(unittest.TestCase):
    """Base class handing each test a temporary repository-like tree."""

    def setUp(self) -> None:
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.base = Path(tmp.name).resolve()

    def add_suite(self, relative: str, source: str = PASSING_SUITE) -> Path:
        suite_dir = self.base / relative
        write(suite_dir / "test_ok.py", source)
        return suite_dir


class DiscoveryTests(TempTreeTestCase):
    def test_discovers_suites_and_excludes_fixture_trees(self) -> None:
        self.add_suite("tools/tests")
        self.add_suite("tools/dashboard/tests")
        self.add_suite("tools/lints/deps/tests")
        self.add_suite("harness/datagen/tests")
        # A real suite that happens to contain fixture data below it stays.
        self.add_suite("tools/gen/tests")
        write(self.base / "tools/gen/tests/corpus/targets/tests/test_leak.py", PASSING_SUITE)
        # Fixture trees: a tests directory reached through corpus/fixtures/data.
        write(self.base / "tools/hyg/tests/fixtures/case/tests/test_leak.py", PASSING_SUITE)
        write(self.base / "harness/datagen/tests/data/deep/tests/test_leak.py", PASSING_SUITE)
        # A directory named tests without any test_*.py file is not a suite.
        write(self.base / "tools/plain/tests/helper.py", "x = 1\n")

        found = run_python_suites.discover_suites(self.base)

        self.assertEqual(
            [suite.name for suite in found],
            [
                "harness/datagen/tests",
                "tools/dashboard/tests",
                "tools/gen/tests",
                "tools/lints/deps/tests",
                "tools/tests",
            ],
        )

    def test_missing_search_root_is_ignored(self) -> None:
        self.add_suite("tools/tests")
        self.assertEqual(
            [suite.name for suite in run_python_suites.discover_suites(self.base)],
            ["tools/tests"],
        )

    def test_list_prints_suites_and_exits_zero(self) -> None:
        self.add_suite("tools/dashboard/tests")
        self.add_suite("tools/tests")
        with captured() as (out, _err):
            code = run_python_suites.main(["--list", "--base", str(self.base)])
        self.assertEqual(code, 0)
        self.assertEqual(out.getvalue().splitlines(), ["tools/dashboard/tests", "tools/tests"])


class RunBehaviourTests(TempTreeTestCase):
    def run_runner(self, *extra: str) -> tuple[int, str, str]:
        with neutral_step_summary(), captured() as (out, err):
            code = run_python_suites.main(["--base", str(self.base), *extra])
        return code, out.getvalue(), err.getvalue()

    def test_failing_suite_fails_the_run(self) -> None:
        self.add_suite("tools/tests", FAILING_SUITE)

        code, out, err = self.run_runner()

        self.assertEqual(code, 1)
        self.assertIn("tools/tests  FAIL", out)
        self.assertIn("FAILED suites: tools/tests", out)
        self.assertIn("1 suites: 0 passed, 1 failed, 0 timed out, 0 skipped", out)

    def test_timeout_is_reported_as_failure(self) -> None:
        self.add_suite("tools/tests", SLOW_SUITE)

        code, out, _err = self.run_runner("--timeout", "3")

        self.assertEqual(code, 1)
        self.assertIn("tools/tests  TIMEOUT", out)
        self.assertIn("timed out after 3.00s", out)
        self.assertIn("FAILED suites: tools/tests", out)

    def test_passing_suite_reports_test_count_and_duration(self) -> None:
        self.add_suite("tools/tests")

        code, out, err = self.run_runner()

        self.assertEqual(code, 0, msg=f"stderr: {err}\nstdout: {out}")
        self.assertRegex(out, r"tools/tests +PASS +1 +\d+\.\d+s")
        self.assertIn("1 suites: 1 passed, 0 failed, 0 timed out, 0 skipped", out)

    def test_empty_discovery_is_an_error(self) -> None:
        code, out, err = self.run_runner()

        self.assertEqual(code, 2)
        self.assertIn("no Python test suites found", err)
        self.assertEqual(out, "")

    def test_list_on_empty_discovery_is_an_error(self) -> None:
        with captured() as (_out, err):
            code = run_python_suites.main(["--list", "--base", str(self.base)])

        self.assertEqual(code, 2)
        self.assertIn("no Python test suites found", err.getvalue())

    def test_step_summary_is_written_when_env_var_set(self) -> None:
        self.add_suite("tools/tests")
        summary = self.base / "step-summary.md"

        with neutral_step_summary():
            with mock.patch.dict(os.environ, {"GITHUB_STEP_SUMMARY": str(summary)}):
                with captured():
                    code = run_python_suites.main(["--base", str(self.base)])

        self.assertEqual(code, 0)
        text = summary.read_text(encoding="utf-8")
        self.assertIn(STEP_SUMMARY_HEADER, text)
        self.assertIn("| suite | result | tests | time | note |", text)
        self.assertIn("| tools/tests | PASS | 1 |", text)

    def test_no_step_summary_is_written_when_env_var_unset(self) -> None:
        self.add_suite("tools/tests")

        code, _out, _err = self.run_runner()

        self.assertEqual(code, 0)
        self.assertFalse((self.base / "GITHUB_STEP_SUMMARY").exists())


class SkipRuleTests(TempTreeTestCase):
    """skip.toml rules: reason required, stale entry is an error, OS matching."""

    def setUp(self) -> None:
        super().setUp()
        self.add_suite("tools/tests")
        self.skip_file = self.base / "tools" / "ci" / "skip.toml"

    def write_skips(self, text: str) -> None:
        write(self.skip_file, text)

    def run_runner(self, *extra: str) -> tuple[int, str, str]:
        with neutral_step_summary(), captured() as (out, err):
            code = run_python_suites.main(["--base", str(self.base), *extra])
        return code, out.getvalue(), err.getvalue()

    def test_missing_skip_file_means_no_skips(self) -> None:
        self.assertEqual(run_python_suites.load_skips(self.skip_file, {"tools/tests"}), {})

    def test_skip_entry_requires_a_reason(self) -> None:
        self.write_skips('windows = [{ suite = "tools/tests" }]\n')
        with self.assertRaises(run_python_suites.SkipConfigError) as ctx:
            run_python_suites.load_skips(self.skip_file, {"tools/tests"})
        self.assertIn("reason", str(ctx.exception))

    def test_blank_reason_is_rejected(self) -> None:
        self.write_skips('linux = [{ suite = "tools/tests", reason = "  " }]\n')
        with self.assertRaises(run_python_suites.SkipConfigError):
            run_python_suites.load_skips(self.skip_file, {"tools/tests"})

    def test_stale_skip_entry_is_an_error(self) -> None:
        self.write_skips('windows = [{ suite = "tools/gone/tests", reason = "old platform bug" }]\n')
        code, _out, err = self.run_runner()

        self.assertEqual(code, 2)
        self.assertIn("stale skip", err)
        self.assertIn("tools/gone/tests", err)

    def test_unknown_os_key_is_an_error(self) -> None:
        self.write_skips('win32 = [{ suite = "tools/tests", reason = "typo" }]\n')
        code, _out, err = self.run_runner()

        self.assertEqual(code, 2)
        self.assertIn("unknown OS key", err)

    def test_invalid_toml_is_an_error(self) -> None:
        self.write_skips("windows = [broken\n")
        code, _out, err = self.run_runner()

        self.assertEqual(code, 2)
        self.assertIn("invalid TOML", err)

    def test_skip_only_applies_to_matching_os(self) -> None:
        self.write_skips('windows = [{ suite = "tools/tests", reason = "windows-only failure" }]\n')
        code, out, _err = self.run_runner()

        self.assertEqual(code, 0)
        if sys.platform == "win32":
            self.assertIn("tools/tests  SKIP", out)
        else:
            self.assertIn("tools/tests  PASS", out)

    def test_skip_for_current_os_is_skipped_with_reason(self) -> None:
        platform = run_python_suites.platform_key()
        self.write_skips(f'{platform} = [{{ suite = "tools/tests", reason = "known platform issue" }}]\n')

        code, out, _err = self.run_runner()

        self.assertEqual(code, 0)
        self.assertIn("tools/tests  SKIP", out)
        self.assertIn("known platform issue", out)
        self.assertIn("1 suites: 0 passed, 0 failed, 0 timed out, 1 skipped", out)


if __name__ == "__main__":
    unittest.main()