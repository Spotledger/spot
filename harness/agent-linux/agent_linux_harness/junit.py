"""Turn libtest output into JUnit XML, so the matrix run is a CI artefact.

The real tests are ordinary Rust tests, and the Rust test harness prints a stable text
summary (``test <name> ... ok|FAILED|ignored``). This module parses that text into
:class:`TestResult` objects and renders one JUnit ``<testsuite>`` per target. Standard
library only (``re``, ``xml.etree.ElementTree``).
"""

from __future__ import annotations

import re
import xml.etree.ElementTree as ET
from dataclasses import dataclass, field

#: ``test some::name ... ok`` / ``FAILED`` / ``ignored``.
_TEST_LINE = re.compile(r"^test (\S+) \.\.\. (ok|FAILED|ignored)\s*$")
#: ``test result: FAILED. 1 passed; 1 failed; 0 ignored; ...; finished in 0.01s``.
_SUMMARY = re.compile(r"^test result: .*finished in ([0-9.]+)s\s*$")

PASSED = "passed"
FAILED = "failed"
SKIPPED = "skipped"
ERROR = "error"


@dataclass
class TestResult:
    """One test's outcome, as the matrix table and the JUnit XML see it."""

    name: str
    status: str
    detail: str = ""
    duration: float = 0.0

    @property
    def ran(self) -> bool:
        """Whether the test actually executed (as opposed to being skipped)."""
        return self.status in (PASSED, FAILED, ERROR)


@dataclass
class TargetReport:
    """Every test result for one target, plus any target-level error."""

    target: str
    results: list[TestResult] = field(default_factory=list)
    error: str = ""

    def counts(self) -> tuple[int, int, int]:
        """``(passed, failed, skipped)``."""
        passed = sum(1 for r in self.results if r.status == PASSED)
        failed = sum(1 for r in self.results if r.status in (FAILED, ERROR))
        skipped = sum(1 for r in self.results if r.status == SKIPPED)
        return passed, failed, skipped


def parse_libtest_output(text: str, duration: float = 0.0) -> dict[str, TestResult]:
    """Parse libtest's text output into a result per test name.

    A ``FAILED`` test gets the whole output as its detail (capped), so the JUnit XML
    carries enough to debug without a second run. The harness's own ``test result:``
    summary supplies a per-run duration only; per-test timings are not printed by libtest,
    so every test is stamped with the run's duration divided evenly, which is honest about
    being an estimate.
    """
    results: dict[str, TestResult] = {}
    failed_names: list[str] = []
    for line in text.splitlines():
        match = _TEST_LINE.match(line)
        if not match:
            continue
        name, verdict = match.group(1), match.group(2)
        if verdict == "ok":
            results[name] = TestResult(name, PASSED)
        elif verdict == "ignored":
            results[name] = TestResult(name, SKIPPED, "ignored")
        else:
            results[name] = TestResult(name, FAILED)
            failed_names.append(name)

    if results:
        per_test = duration / len(results) if duration else 0.0
        for result in results.values():
            result.duration = per_test

    if failed_names:
        detail = text.strip()
        if len(detail) > 20_000:
            detail = detail[:20_000] + "\n… (truncated)"
        for name in failed_names:
            results[name].detail = detail

    return results


def run_duration(text: str) -> float:
    """The duration libtest reported for a run, or ``0.0`` when it printed none."""
    for line in text.splitlines():
        match = _SUMMARY.match(line)
        if match:
            try:
                return float(match.group(1))
            except ValueError:
                return 0.0
    return 0.0


def render_junit(reports: list[TargetReport]) -> str:
    """Render the target reports as a JUnit XML document string."""
    root = ET.Element("testsuites")
    total_tests = sum(len(report.results) for report in reports)
    total_failures = sum(report.counts()[1] for report in reports)
    root.set("name", "spot-agent-linux")
    root.set("tests", str(total_tests))
    root.set("failures", str(total_failures))

    for report in reports:
        passed, failed, skipped = report.counts()
        suite = ET.SubElement(root, "testsuite")
        suite.set("name", report.target)
        suite.set("tests", str(len(report.results)))
        suite.set("failures", str(failed))
        suite.set("skipped", str(skipped))
        suite.set("passed", str(passed))
        if report.error:
            error_case = ET.SubElement(suite, "testcase")
            error_case.set("name", "target-setup")
            error_case.set("classname", report.target)
            error = ET.SubElement(error_case, "error")
            error.set("message", report.error)

        for result in report.results:
            case = ET.SubElement(suite, "testcase")
            case.set("name", result.name)
            case.set("classname", report.target)
            case.set("time", f"{result.duration:.3f}")
            if result.status == FAILED:
                failure = ET.SubElement(case, "failure")
                failure.set("message", f"{result.name} failed")
                failure.text = result.detail or "no detail captured"
            elif result.status == ERROR:
                error = ET.SubElement(case, "error")
                error.set("message", result.detail or "error")
            elif result.status == SKIPPED:
                skip = ET.SubElement(case, "skipped")
                skip.set("message", result.detail or "skipped")

    _indent(root)
    return '<?xml version="1.0" encoding="UTF-8"?>\n' + ET.tostring(
        root, encoding="unicode"
    )


def _indent(element: ET.Element, level: int = 0) -> None:
    """Indent an ElementTree in place (``ET.indent`` exists on 3.9+, this keeps it explicit)."""
    pad = "\n" + "  " * level
    if len(element):
        if not element.text or not element.text.strip():
            element.text = pad + "  "
        for child in element:
            _indent(child, level + 1)
        if not element.tail or not element.tail.strip():
            element.tail = pad
    elif level and (not element.tail or not element.tail.strip()):
        element.tail = pad
