"""Tests for tools/security/audit.py (WP-PRD-08.T2).

Everything runs against committed fixture outputs, so neither cargo-deny nor
dotnet nor a network is needed; the external tools are stubbed at the
``run_tool`` boundary. Covers: both parsers, the severity threshold, allowlist
suppression and expiry, tool-error/skip/strict exit codes and the CLI wiring.
"""
from __future__ import annotations

import contextlib
import datetime
import io
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE.parent))

import audit  # noqa: E402

FIXTURES = HERE / "fixtures"
CARGO_FIXTURE = FIXTURES / "cargo_deny_findings.txt"
DOTNET_FIXTURE = FIXTURES / "dotnet_vulnerable.txt"
SOLUTION_FIXTURE = FIXTURES / "fixture.sln"
ALLOWLIST_FIXTURE = FIXTURES / "allowlist.toml"


def fixture(name: str) -> str:
    return (FIXTURES / name).read_text(encoding="utf-8")


def stub_run_tool(cargo: audit.ToolResult, dotnet: audit.ToolResult):
    """Replace the tool invocations with canned ToolResults."""
    def _stub(name: str, argv: list[str], cwd: Path) -> audit.ToolResult:
        return cargo if name == "cargo-deny" else dotnet
    return _stub


class NormaliseSeverityTest(unittest.TestCase):
    def test_maps_tool_wording(self):
        for raw, expected in [("High", "high"), ("Moderate", "medium"),
                              ("7.5 (High)", "high"), ("9.8", "critical"),
                              ("5.0", "medium"), ("0.0", "low"), ("None", "low"),
                              ("Top-level", "low"), ("", "low")]:
            with self.subTest(raw=raw):
                self.assertEqual(audit.normalise_severity(raw), expected)


class ParseCargoDenyTest(unittest.TestCase):
    def setUp(self):
        self.findings = audit.parse_cargo_deny(fixture("cargo_deny_findings.txt"))

    def test_finds_every_block(self):
        self.assertEqual(len(self.findings), 3)

    def test_vulnerability_fields(self):
        idna = self.findings[0]
        self.assertEqual(idna.tool, "cargo-deny")
        self.assertEqual(idna.check, "advisory")
        self.assertEqual(idna.finding, "RUSTSEC-2024-0421")
        self.assertEqual(idna.package, "idna")
        self.assertEqual(idna.version, "0.1.5")
        self.assertEqual(idna.severity, "high")
        self.assertEqual(idna.fixed_in, "0.5.0")
        self.assertTrue(idna.url.endswith("/RUSTSEC-2024-0421"))
        self.assertTrue(idna.where.endswith("Cargo.lock:198:3"))

    def test_warning_findings_grade_low_with_no_fixed_version(self):
        ammonia = self.findings[1]
        self.assertEqual(ammonia.severity, "low")
        self.assertEqual(ammonia.finding, "RUSTSEC-2019-0002")
        self.assertEqual(ammonia.fixed_in, "")

    def test_ban_findings_keep_the_deny_code(self):
        leftpad = self.findings[2]
        self.assertEqual(leftpad.check, "ban")
        self.assertEqual(leftpad.finding, "B0001")
        self.assertEqual(leftpad.severity, "high")
        self.assertEqual(leftpad.fixed_in, "")

    def test_empty_and_noise_output_parse_to_nothing(self):
        self.assertEqual(audit.parse_cargo_deny(""), [])
        self.assertEqual(audit.parse_cargo_deny("   Compiling spot-runtime v0.1.0\n"), [])


class ParseDotnetTest(unittest.TestCase):
    def setUp(self):
        self.findings = audit.parse_dotnet_vulnerable(fixture("dotnet_vulnerable.txt"))

    def test_finds_every_vulnerable_package(self):
        self.assertEqual(len(self.findings), 3)

    def test_transitive_package_fields(self):
        sql_client = self.findings[0]
        self.assertEqual(sql_client.tool, "dotnet")
        self.assertEqual(sql_client.check, "vulnerable")
        self.assertEqual(sql_client.finding, "GHSA-8g2q-pp82-688v")
        self.assertEqual(sql_client.package, "Microsoft.Data.SqlClient")
        self.assertEqual(sql_client.version, "5.1.1")
        self.assertEqual(sql_client.severity, "high")
        self.assertEqual(sql_client.where, "[net9.0] Spotledger.Client")

    def test_threat_levels_map_to_severities(self):
        self.assertEqual(self.findings[1].severity, "medium")
        self.assertEqual(self.findings[1].package, "System.Text.Json")
        self.assertEqual(self.findings[2].severity, "critical")
        self.assertEqual(self.findings[2].finding, "Moq")  # no GHSA in the output

    def test_projects_without_findings_are_ignored(self):
        # the fixture contains a "has no vulnerable packages" section for Client
        self.assertFalse(any(f.package == "-" for f in self.findings))

class SeverityThresholdTest(unittest.TestCase):
    def test_severity_at_or_above(self):
        cases = [("high", "high", True), ("medium", "high", False), ("critical", "high", True),
                 ("low", "low", True), ("medium", "low", True), ("low", "medium", False),
                 ("moderate", "medium", True), ("unknown tool says what?", "high", False)]
        for severity, threshold, expected in cases:
            with self.subTest(severity=severity, threshold=threshold):
                self.assertEqual(audit.severity_at_or_above(severity, threshold), expected)

    def test_exit_code_precedence(self):
        clean = dict(tool_errors=0, missing=0, strict=False, failing=0, expired=0)
        self.assertEqual(audit.audit_exit_code(**clean), 0)
        self.assertEqual(audit.audit_exit_code(**{**clean, "failing": 1}), 1)
        self.assertEqual(audit.audit_exit_code(**{**clean, "expired": 1}), 1)
        self.assertEqual(audit.audit_exit_code(**{**clean, "failing": 1, "expired": 1}), 1)
        self.assertEqual(audit.audit_exit_code(**{**clean, "failing": 1, "tool_errors": 1}), 2)
        self.assertEqual(audit.audit_exit_code(**{**clean, "missing": 1, "strict": True}), 2)
        self.assertEqual(audit.audit_exit_code(**{**clean, "missing": 1, "strict": False}), 0)
        self.assertEqual(audit.audit_exit_code(**{**clean, "tool_errors": 1,
                                                  "missing": 1, "strict": False}), 2)


class AllowlistTest(unittest.TestCase):
    def setUp(self):
        self.entries = audit.load_allowlist(ALLOWLIST_FIXTURE)
        self.today = datetime.date(2026, 6, 15)

    def test_expects_the_documented_keys(self):
        for entry in self.entries:
            self.assertEqual(set(entry), set(audit.REQUIRED_EXCEPTION_KEYS))
        self.assertEqual([str(entry["expires"]) for entry in self.entries],
                         ["2999-12-31", "2020-01-31"])

    def test_missing_file_is_empty_and_broken_entries_are_rejected(self):
        self.assertEqual([], audit.load_allowlist(HERE / "fixtures" / "does-not-exist.toml"))
        case_files = {
            "missing id": "[[exceptions]]\npackage = \"p\"\nreason = \"r\"\n"
                          "owner = \"lead\"\nexpires = \"2026-01-01\"\n",
            "bad date": "[[exceptions]]\nid = \"A\"\npackage = \"p\"\nreason = \"r\"\n"
                        "owner = \"lead\"\nexpires = \"not-a-date\"\n",
            "bad toml": "[[exceptions\n",
        }
        for name, text in case_files.items():
            with self.subTest(case=name):
                with tempfile.TemporaryDirectory() as tmp:
                    path = Path(tmp) / "allowlist.toml"
                    path.write_text(text, encoding="utf-8")
                    with self.assertRaises(audit.AllowlistError):
                        audit.load_allowlist(path)

    def test_expiry_boundary_and_findings(self):
        entry = {"id": "X", "package": "p", "reason": "r", "owner": "lead",
                 "expires": "2026-06-15"}
        self.assertFalse(audit.exception_expired(entry, self.today))
        self.assertTrue(audit.exception_expired(entry, datetime.date(2026, 6, 16)))
        expired = audit.expiry_findings(self.entries)
        self.assertEqual(len(expired), 1)
        self.assertEqual(expired[0].check, "expired-exception")
        self.assertEqual(expired[0].package, "System.Text.Json")
        self.assertEqual(expired[0].severity, "high")

    def test_suppress_matches_id_or_package_of_active_entries_only(self):
        findings = audit.parse_cargo_deny(fixture("cargo_deny_findings.txt"))
        audit.suppress(findings, self.entries, self.today)
        self.assertTrue(findings[1].suppressed)   # ammonia, by id and package
        self.assertFalse(findings[0].suppressed)  # idna, nothing matches
        self.assertFalse(findings[2].suppressed)  # leftpad, nothing matches


class AuditFlowTest(unittest.TestCase):
    """audit() end to end with the external tools stubbed out."""

    def run_audit(self, *, cargo: audit.ToolResult, dotnet: audit.ToolResult,
                  allowlist: Path | None = None, strict: bool = False,
                  fail_on: str = "high", solution: Path = SOLUTION_FIXTURE):
        stderr = io.StringIO()
        with contextlib.redirect_stdout(io.StringIO()), \
                contextlib.redirect_stderr(stderr), \
                mock.patch.object(audit, "run_tool", stub_run_tool(cargo, dotnet)):
            code, findings = audit.audit(HERE, fail_on=fail_on, strict=strict,
                                         allowlist_path=allowlist, solution=solution)
        return code, findings, stderr.getvalue()

    def test_clean_audit_when_both_tools_are_missing_and_not_strict(self):
        code, findings, stderr = self.run_audit(cargo=audit.ToolResult("cargo-deny", "missing"),
                                                dotnet=audit.ToolResult("dotnet", "missing"))
        self.assertEqual(code, 0)
        self.assertEqual(findings, [])
        self.assertEqual(stderr, "")

    def test_strict_fails_when_a_tool_is_missing(self):
        code, _, _ = self.run_audit(cargo=audit.ToolResult("cargo-deny", "missing"),
                                    dotnet=audit.ToolResult("dotnet", "missing"), strict=True)
        self.assertEqual(code, 2)

    def test_findings_above_the_threshold_fail(self):
        cargo = audit.ToolResult("cargo-deny", "ran", 4, fixture("cargo_deny_findings.txt"))
        code, findings, _ = self.run_audit(cargo=cargo, dotnet=audit.ToolResult("dotnet", "missing"))
        self.assertEqual(code, 1)
        self.assertEqual(len(findings), 3)

    def test_medium_findings_pass_at_the_default_threshold(self):
        cargo = audit.ToolResult("cargo-deny", "ran", 0, "   Compiling spot-runtime v0.1.0\n")
        code, findings, _ = self.run_audit(cargo=cargo, dotnet=audit.ToolResult("dotnet", "missing"))
        self.assertEqual(code, 0)
        self.assertEqual(findings, [])

    def test_suppressed_findings_do_not_fail_the_audit(self):
        with tempfile.TemporaryDirectory() as tmp:
            allowlist = Path(tmp) / "allowlist.toml"
            allowlist.write_text(
                "[[exceptions]]\nid = \"X-1\"\npackage = \"idna\"\nreason = \"test\"\n"
                "owner = \"lead\"\nexpires = \"2999-12-31\"\n"
                "[[exceptions]]\nid = \"X-2\"\npackage = \"leftpad\"\nreason = \"test\"\n"
                "owner = \"lead\"\nexpires = \"2999-12-31\"\n", encoding="utf-8")
            cargo = audit.ToolResult("cargo-deny", "ran", 4, fixture("cargo_deny_findings.txt"))
            code, findings, _ = self.run_audit(cargo=cargo, allowlist=allowlist,
                                               dotnet=audit.ToolResult("dotnet", "missing"))
            self.assertEqual(code, 0)
            # every finding severe enough to fail was covered; ammonia stays unsuppressed
            self.assertTrue(findings[0].suppressed and findings[2].suppressed)
            self.assertFalse(findings[1].suppressed)

    def test_expired_exception_fails_even_without_matching_findings(self):
        with tempfile.TemporaryDirectory() as tmp:
            allowlist = Path(tmp) / "allowlist.toml"
            allowlist.write_text(
                "[[exceptions]]\nid = \"OLD-1\"\npackage = \"never-found\"\nreason = \"stale\"\n"
                "owner = \"lead\"\nexpires = \"2020-01-31\"\n", encoding="utf-8")
            quiet = audit.ToolResult("cargo-deny", "ran", 0,
                                     "warning[A0009]: Unmaintained crate\n"
                                     " ├─ Package: olddep\n ├─ Version: 1.0.0\n")
            stdout = io.StringIO()
            with contextlib.redirect_stdout(stdout), \
                    mock.patch.object(audit, "run_tool", stub_run_tool(quiet, audit.ToolResult("dotnet", "missing"))):
                code, findings = audit.audit(HERE, fail_on="high", allowlist_path=allowlist,
                                             solution=SOLUTION_FIXTURE)
            self.assertEqual(code, 1)  # low-severity warning alone would pass
            self.assertIn("expired exception", stdout.getvalue())

    def test_broken_allowlist_fails_closed(self):
        with tempfile.TemporaryDirectory() as tmp:
            allowlist = Path(tmp) / "allowlist.toml"
            allowlist.write_text("[[exceptions\n", encoding="utf-8")
            code, findings, stderr = self.run_audit(cargo=audit.ToolResult("cargo-deny", "missing"),
                                                    dotnet=audit.ToolResult("dotnet", "missing"),
                                                    allowlist=allowlist)
            self.assertEqual(code, 2)
            self.assertEqual(findings, [])
            self.assertIn("unreadable TOML", stderr)

    def test_tool_error_takes_precedence_and_returns_2(self):
        cargo = audit.ToolResult("cargo-deny", "failed", 0, "cargo-deny went away")
        code, findings, stderr = self.run_audit(cargo=cargo, dotnet=audit.ToolResult("dotnet", "missing"))
        self.assertEqual(code, 2)
        self.assertEqual(findings, [])
        self.assertIn("cargo-deny went away", stderr)

    def test_missing_solution_is_a_tool_error(self):
        code, _, stderr = self.run_audit(cargo=audit.ToolResult("cargo-deny", "missing"),
                                         dotnet=audit.ToolResult("dotnet", "missing"),
                                         solution=HERE / "fixtures" / "no-such.sln")
        self.assertEqual(code, 2)
        self.assertIn("solution not found", stderr)

    def test_solution_with_no_projects_is_a_tool_error(self):
        with tempfile.TemporaryDirectory() as tmp:
            empty = Path(tmp) / "empty.sln"
            empty.write_text("Microsoft Visual Studio Solution File\n", encoding="utf-8")
            code, _, stderr = self.run_audit(cargo=audit.ToolResult("cargo-deny", "missing"),
                                             dotnet=audit.ToolResult("dotnet", "missing"),
                                             solution=empty)
            self.assertEqual(code, 2)
            self.assertIn("no .csproj project found", stderr)

    def test_dotnet_gets_the_project_path_joined_to_the_solution_directory(self):
        """Regression: .sln project paths are relative to the .sln, not the root."""
        calls = []

        def recording(_name, argv, _cwd):
            calls.append([str(part) for part in argv])
            return audit.ToolResult("dotnet", "missing")

        with contextlib.redirect_stdout(io.StringIO()), \
                mock.patch.object(audit, "run_tool", recording):
            code, _ = audit.audit(HERE, solution=SOLUTION_FIXTURE)
        self.assertEqual(code, 0)  # tools missing, not strict
        dotnet_paths = [call[2] for call in calls if call[0] == "dotnet"]
        self.assertEqual(Path(dotnet_paths[0]).resolve(),
                         (SOLUTION_FIXTURE.parent / "src" / "Spotledger.Client"
                          / "Spotledger.Client.csproj").resolve())


class SolutionParsingTest(unittest.TestCase):
    def test_finds_projects_in_fixture_sln(self):
        projects = audit.parse_solution(SOLUTION_FIXTURE)
        names = [p.name for p in projects]
        self.assertEqual(names, ["Spotledger.Client.csproj", "Spotledger.Client.Tests.csproj"])
        self.assertEqual(projects[0], Path("src/Spotledger.Client/Spotledger.Client.csproj"))


class CliTest(unittest.TestCase):
    def test_help_exits_zero(self):
        proc = subprocess.run([sys.executable, str(HERE.parent / "audit.py"), "--help"],
                              capture_output=True, text=True, timeout=60)
        self.assertEqual(proc.returncode, 0)
        self.assertIn("--fail-on", proc.stdout)

    def test_main_maps_arguments_to_audit(self):
        cargo = audit.ToolResult("cargo-deny", "ran", 4, fixture("cargo_deny_findings.txt"))
        with contextlib.redirect_stdout(io.StringIO()), \
                mock.patch.object(audit, "run_tool", stub_run_tool(cargo, audit.ToolResult("dotnet", "missing"))):
            code = audit.main(["--fail-on", "moderate", "--allowlist", str(ALLOWLIST_FIXTURE),
                               "--solution", str(SOLUTION_FIXTURE), "--root", str(HERE)])
        # cargo-deny runs; idna (high) and leftpad (high, ban) exceed medium; the
        # allowlist also carries an expired exception.
        self.assertEqual(code, 1)


if __name__ == "__main__":
    unittest.main()