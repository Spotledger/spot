"""Tests for the command line: listing, dry runs, and the refusal paths."""

import contextlib
import io
import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

from agent_linux_harness.cli import build_parser, run  # noqa: E402
from agent_linux_harness.runner import StubRunner  # noqa: E402


def invoke(*argv):
    """Run the CLI over a stub runner, returning ``(exit_code, stdout, stderr)``."""
    args = build_parser().parse_args(list(argv))
    out, err = io.StringIO(), io.StringIO()
    with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
        code = run(args, StubRunner())
    return code, out.getvalue(), err.getvalue()


class CliTests(unittest.TestCase):
    def test_list_needs_no_guard(self):
        code, out, _err = invoke("--list")
        self.assertEqual(code, 0)
        self.assertIn("fedora-44", out)
        self.assertIn("distro_family_matches_target", out)

    def test_dry_run_needs_no_guard(self):
        code, out, _err = invoke("--dry-run")
        self.assertEqual(code, 0)
        self.assertIn("test run(s) planned", out)

    def test_a_dry_run_of_a_single_target(self):
        code, out, _err = invoke("--dry-run", "--target", "ubuntu-2404")
        self.assertEqual(code, 0)
        self.assertIn("ubuntu-2404", out)
        self.assertNotIn("almalinux-10", out)

    def test_an_unknown_target_is_refused(self):
        code, _out, err = invoke("--dry-run", "--target", "nope")
        self.assertEqual(code, 2)
        self.assertIn("unknown target", err)

    def test_execution_refuses_on_a_bare_metal_host(self):
        # No SPOT_HARNESS_DISPOSABLE and a stub root with no container/VM markers.
        code, _out, err = invoke("--target", "fedora-44")
        self.assertEqual(code, 2)
        self.assertIn("SPOT_HARNESS_DISPOSABLE", err)

    def test_vm_mode_is_not_executed_by_the_container_cli(self):
        code, _out, err = invoke("--mode", "vm")
        self.assertEqual(code, 2)
        self.assertIn("vm/run.sh", err)

    def test_a_dry_run_of_vm_mode_is_allowed(self):
        code, out, _err = invoke("--mode", "vm", "--dry-run")
        self.assertEqual(code, 0)
        self.assertIn("fedora-44-vm", out)


if __name__ == "__main__":
    unittest.main()
