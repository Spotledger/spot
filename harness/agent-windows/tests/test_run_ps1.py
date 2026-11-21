"""Structural tests for the PowerShell runner (WP-ADM-05.T3).

`run.ps1` cannot execute on the Linux CI host (no Windows, no `pwsh`, and its safety
guard would refuse anyway), so these tests check the properties the checked-in script
must have: the two safety guards, the throw-away prefix discipline, the `finally`
cleanup, the cargo crate it drives and the JUnit it writes. They are deliberately about
the contract, not the exact wording, so a rename does not break them but removing a
guard does.
"""

from __future__ import annotations

import re
import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

import orchestrator  # noqa: E402

RUN_PS1 = orchestrator.RUN_PS1


class TestRunnerScript(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.text = RUN_PS1.read_text(encoding="utf-8")
        cls.lines = cls.text.splitlines()

    def test_file_exists_and_is_utf8(self) -> None:
        self.assertTrue(RUN_PS1.is_file())
        self.assertGreater(len(self.text), 1000)

    def test_declares_powershell_seven(self) -> None:
        self.assertTrue(self.text.startswith("#Requires -Version 7.0"))

    def test_strict_mode_and_stop_on_error(self) -> None:
        self.assertIn("Set-StrictMode -Version Latest", self.text)
        self.assertRegex(self.text, r"\$ErrorActionPreference\s*=\s*'Stop'")

    def test_requires_the_disposable_environment_variable(self) -> None:
        self.assertIn("SPOT_HARNESS_DISPOSABLE", self.text)
        self.assertRegex(self.text, r"SPOT_HARNESS_DISPOSABLE\s*-ne\s*'1'")

    def test_checks_the_machine_is_a_virtual_machine(self) -> None:
        self.assertIn("Win32_ComputerSystem", self.text)
        self.assertRegex(self.text, r"\$system\.Model")
        self.assertRegex(self.text, r"Virtual\|VMware\|VirtualBox\|KVM")

    def test_guards_run_before_any_test(self) -> None:
        call_lines = [index for index, line in enumerate(self.lines) if line.strip() == "Assert-Disposable"]
        self.assertEqual(len(call_lines), 1, "exactly one Assert-Disposable call")
        build = next(index for index, line in enumerate(self.lines) if "Invoke-CargoBuild" in line and line.strip().startswith("if"))
        self.assertLess(call_lines[0], build, "the guard must run before the build")

    def test_runs_the_standalone_crate(self) -> None:
        self.assertIn("spot-agent-windows-real", self.text)
        self.assertIn("tests-real", self.text)
        self.assertIn("cargo test", self.text)
        self.assertIn("--test-threads=1", self.text)

    def test_reads_the_matrix_through_the_orchestrator(self) -> None:
        self.assertIn("orchestrator.py", self.text)
        self.assertIn("'matrix'", self.text)
        self.assertIn("'--json'", self.text)

    def test_generates_a_short_unique_prefix(self) -> None:
        self.assertRegex(self.text, r"\$PrefixRoot\s*=\s*'spot-h-'")
        self.assertRegex(self.text, r"Get-Random")
        self.assertIn("SPOT_HARNESS_PREFIX", self.text)

    def test_sweeps_only_prefixed_objects(self) -> None:
        self.assertIn("Remove-Leftovers", self.text)
        self.assertRegex(self.text, r"\.Name\.StartsWith\(\$Prefix\)")
        self.assertRegex(self.text, r"\.DisplayName\.StartsWith\(\$Prefix\)")
        self.assertIn("Remove-LocalUser", self.text)
        self.assertIn("Remove-NetFirewallRule", self.text)
        self.assertIn("sc.exe", self.text)

    def test_cleanup_is_in_a_finally(self) -> None:
        self.assertRegex(self.text, r"finally\s*\{")
        self.assertRegex(self.text, r"Remove-Leftovers -Prefix \$prefix")

    def test_writes_junit_xml(self) -> None:
        self.assertIn("testsuite", self.text)
        self.assertIn("testcase", self.text)
        self.assertRegex(self.text, r"\.xml")
        self.assertRegex(self.text, r"XmlWriter")

    def test_emits_a_json_outcome(self) -> None:
        self.assertIn("ConvertTo-Json", self.text)
        self.assertIn("junit_path", self.text)

    def test_never_builds_a_command_string(self) -> None:
        # The suite must not shell out through a string built from input.
        self.assertNotIn("Invoke-Expression", self.text)

    def test_capability_probes_cover_the_matrix_capabilities(self) -> None:
        matrix = orchestrator.load_matrix()
        for capability in matrix.capability_names:
            self.assertIn(f"'{capability}'", self.text, f"no probe for capability {capability!r}")

    def test_exit_code_reflects_failure(self) -> None:
        self.assertRegex(self.text, r"exit 1")
        self.assertRegex(self.text, r"exit 0")


if __name__ == "__main__":
    unittest.main()
