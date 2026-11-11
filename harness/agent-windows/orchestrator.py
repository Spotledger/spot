#!/usr/bin/env python3
"""Orchestration for the Windows module conformance suite (WP-ADM-05.T3).

WP-ADM-05's acceptance is "module suite green on Windows Server, Windows 10 and 11".
`matrix.toml` is the checked-in definition of that matrix; this module owns the part of
the suite that must behave identically everywhere and can therefore be tested on the
Linux CI host:

* reading and validating `matrix.toml` (``load_matrix``);
* deciding which target/test pairs are allowed to run, so a test whose capabilities a
  target does not offer is *skipped*, never silently passed
  (``Matrix.missing_capabilities``, ``run_matrix``);
* driving a ``Runner`` and aggregating its outcomes into one JUnit XML document and a
  printable matrix (``MatrixReport.to_junit``, ``MatrixReport.render``).

The real work -- building and running the ``spot-agent-windows-real`` cargo crate on a
Windows machine -- is done by ``run.ps1`` on a disposable Windows VM runner. ``PwshRunner`` is the production ``Runner``: it shells out
to ``pwsh run.ps1 -Json``. Tests inject a stub runner instead, so this whole module is
exercised on Linux with no Windows, no ``pwsh`` and no cargo.

The command line mirrors the two things a caller wants:

    python orchestrator.py matrix [--json]
    python orchestrator.py run [--target NAME]... [--test NAME]... [--artifacts DIR]
"""

from __future__ import annotations

import argparse
import dataclasses
import json
import subprocess
import sys
import tomllib
import xml.etree.ElementTree as ET
from pathlib import Path
from typing import Iterable, Protocol, Sequence

#: This file's directory: the harness root.
HARNESS_DIR = Path(__file__).resolve().parent
#: The repository root (``harness/agent-windows`` -> ``harness`` -> repo root).
REPO_ROOT = HARNESS_DIR.parent.parent
#: The checked-in matrix definition.
MATRIX_PATH = HARNESS_DIR / "matrix.toml"
#: The standalone cargo crate the real tests live in.
TESTS_REAL_DIR = HARNESS_DIR / "tests-real"
#: The PowerShell runner.
RUN_PS1 = HARNESS_DIR / "run.ps1"

SCHEMA = 1

STATUS_PASS = "pass"
STATUS_FAIL = "fail"
STATUS_SKIP = "skip"
STATUS_ERROR = "error"
STATUSES = (STATUS_PASS, STATUS_FAIL, STATUS_SKIP, STATUS_ERROR)

#: The capability names every target in the matrix must offer (WP-ADM-05.T3, deliverable 1).
REQUIRED_CAPABILITIES = ("admin", "vss", "update_service", "firewall", "netapi")


class MatrixError(Exception):
    """``matrix.toml`` is missing, malformed or internally inconsistent."""


@dataclasses.dataclass(frozen=True)
class Capability:
    """One capability a target may offer, with how ``run.ps1`` probes it."""

    name: str
    description: str
    probe: str


@dataclasses.dataclass(frozen=True)
class Target:
    """One operating system the real suite runs on."""

    name: str
    os: str
    version: str
    edition: str
    image: str
    runner_labels: tuple[str, ...]
    capabilities: tuple[str, ...]


@dataclasses.dataclass(frozen=True)
class RealTest:
    """One real-system cargo integration test."""

    name: str
    file: str
    description: str
    capabilities: tuple[str, ...]


@dataclasses.dataclass(frozen=True)
class Matrix:
    """The parsed, validated matrix."""

    schema: int
    module: str
    wp: str
    capabilities: tuple[Capability, ...]
    targets: tuple[Target, ...]
    tests: tuple[RealTest, ...]

    @property
    def capability_names(self) -> tuple[str, ...]:
        return tuple(capability.name for capability in self.capabilities)

    def capability(self, name: str) -> Capability:
        """The capability called ``name``.

        Raises:
            MatrixError: when the matrix declares no such capability.
        """
        for capability in self.capabilities:
            if capability.name == name:
                return capability
        raise MatrixError(f"unknown capability {name!r}")

    def target(self, name: str) -> Target:
        """The target called ``name``.

        Raises:
            MatrixError: when the matrix declares no such target.
        """
        for target in self.targets:
            if target.name == name:
                return target
        raise MatrixError(f"unknown target {name!r}")

    def real_test(self, name: str) -> RealTest:
        """The test called ``name``.

        Raises:
            MatrixError: when the matrix declares no such test.
        """
        for test in self.tests:
            if test.name == name:
                return test
        raise MatrixError(f"unknown test {name!r}")

    def missing_capabilities(self, target: Target, test: RealTest) -> tuple[str, ...]:
        """The capabilities ``test`` needs that ``target`` does not offer."""
        offered = set(target.capabilities)
        return tuple(name for name in test.capabilities if name not in offered)

    def select_targets(self, names: Iterable[str] | None = None) -> tuple[Target, ...]:
        """The named targets, or every target when ``names`` is empty."""
        wanted = tuple(names or ())
        if not wanted:
            return self.targets
        return tuple(self.target(name) for name in wanted)

    def select_tests(self, names: Iterable[str] | None = None) -> tuple[RealTest, ...]:
        """The named tests, or every test when ``names`` is empty."""
        wanted = tuple(names or ())
        if not wanted:
            return self.tests
        return tuple(self.real_test(name) for name in wanted)

    def test_path(self, test: RealTest) -> Path:
        """The absolute path of a test's source file (relative to the harness dir)."""
        return HARNESS_DIR / test.file


def _require(mapping: dict, key: str, where: str) -> object:
    if key not in mapping:
        raise MatrixError(f"{where}: missing required key {key!r}")
    return mapping[key]


def _require_str(mapping: dict, key: str, where: str) -> str:
    value = _require(mapping, key, where)
    if not isinstance(value, str) or not value.strip():
        raise MatrixError(f"{where}: {key!r} must be a non-empty string")
    return value


def _require_str_list(mapping: dict, key: str, where: str) -> tuple[str, ...]:
    value = _require(mapping, key, where)
    if not isinstance(value, list) or not value:
        raise MatrixError(f"{where}: {key!r} must be a non-empty array of strings")
    if not all(isinstance(item, str) and item for item in value):
        raise MatrixError(f"{where}: {key!r} must contain only non-empty strings")
    return tuple(value)


def load_matrix(path: Path | str = MATRIX_PATH) -> Matrix:
    """Read and validate ``matrix.toml``.

    Raises:
        MatrixError: when the file is missing, not TOML, or internally inconsistent.
    """
    path = Path(path)
    try:
        with open(path, "rb") as handle:
            data = tomllib.load(handle)
    except FileNotFoundError as exc:
        raise MatrixError(f"{path}: no such file") from exc
    except tomllib.TOMLDecodeError as exc:
        raise MatrixError(f"{path}: invalid TOML: {exc}") from exc

    if not isinstance(data, dict):
        raise MatrixError(f"{path}: the document must be a table")

    schema = _require(data, "schema", str(path))
    if schema != SCHEMA:
        raise MatrixError(f"{path}: unsupported schema {schema!r} (expected {SCHEMA})")

    raw_capabilities = _require(data, "capabilities", str(path))
    if not isinstance(raw_capabilities, dict) or not raw_capabilities:
        raise MatrixError(f"{path}: [capabilities] must be a non-empty table")
    capabilities: list[Capability] = []
    for name, body in raw_capabilities.items():
        if not isinstance(body, dict):
            raise MatrixError(f"{path}: [capabilities.{name}] must be a table")
        where = f"[capabilities.{name}]"
        capabilities.append(
            Capability(
                name=name,
                description=_require_str(body, "description", where),
                probe=_require_str(body, "probe", where),
            )
        )
    known_capabilities = {capability.name for capability in capabilities}

    raw_targets = _require(data, "targets", str(path))
    if not isinstance(raw_targets, list) or not raw_targets:
        raise MatrixError(f"{path}: [[targets]] must be a non-empty array of tables")
    targets: list[Target] = []
    seen_targets: set[str] = set()
    for index, body in enumerate(raw_targets):
        if not isinstance(body, dict):
            raise MatrixError(f"{path}: [[targets]] entry {index} must be a table")
        where = f"[[targets]] entry {index}"
        name = _require_str(body, "name", where)
        if name in seen_targets:
            raise MatrixError(f"{path}: duplicate target name {name!r}")
        seen_targets.add(name)
        capabilities_for_target = _require_str_list(body, "capabilities", where)
        unknown = [c for c in capabilities_for_target if c not in known_capabilities]
        if unknown:
            raise MatrixError(
                f"{path}: target {name!r} names unknown capabilities: {', '.join(unknown)}"
            )
        targets.append(
            Target(
                name=name,
                os=_require_str(body, "os", where),
                version=_require_str(body, "version", where),
                edition=_require_str(body, "edition", where),
                image=_require_str(body, "image", where),
                runner_labels=_require_str_list(body, "runner_labels", where),
                capabilities=capabilities_for_target,
            )
        )

    raw_tests = _require(data, "tests", str(path))
    if not isinstance(raw_tests, list) or not raw_tests:
        raise MatrixError(f"{path}: [[tests]] must be a non-empty array of tables")
    tests: list[RealTest] = []
    seen_tests: set[str] = set()
    for index, body in enumerate(raw_tests):
        if not isinstance(body, dict):
            raise MatrixError(f"{path}: [[tests]] entry {index} must be a table")
        where = f"[[tests]] entry {index}"
        name = _require_str(body, "name", where)
        if name in seen_tests:
            raise MatrixError(f"{path}: duplicate test name {name!r}")
        seen_tests.add(name)
        capabilities_for_test = _require_str_list(body, "capabilities", where)
        unknown = [c for c in capabilities_for_test if c not in known_capabilities]
        if unknown:
            raise MatrixError(
                f"{path}: test {name!r} names unknown capabilities: {', '.join(unknown)}"
            )
        tests.append(
            RealTest(
                name=name,
                file=_require_str(body, "file", where),
                description=_require_str(body, "description", where),
                capabilities=capabilities_for_test,
            )
        )

    return Matrix(
        schema=schema,
        module=_require_str(data, "module", str(path)),
        wp=_require_str(data, "wp", str(path)),
        capabilities=tuple(capabilities),
        targets=tuple(targets),
        tests=tuple(tests),
    )


def matrix_to_dict(matrix: Matrix) -> dict:
    """A JSON-serialisable view of the matrix (used by ``run.ps1`` and the CLI)."""
    return {
        "schema": matrix.schema,
        "module": matrix.module,
        "wp": matrix.wp,
        "capabilities": [dataclasses.asdict(capability) for capability in matrix.capabilities],
        "targets": [dataclasses.asdict(target) for target in matrix.targets],
        "tests": [dataclasses.asdict(test) for test in matrix.tests],
    }


@dataclasses.dataclass(frozen=True)
class RunOutcome:
    """The result of running one test on one target."""

    target: str
    test: str
    status: str
    passed: int = 0
    failed: int = 0
    skipped: int = 0
    message: str = ""
    junit: str = ""

    def __post_init__(self) -> None:
        if self.status not in STATUSES:
            raise ValueError(f"unknown status {self.status!r}")


class Runner(Protocol):
    """What the orchestrator needs from a runner: run one test on one target."""

    def run(self, target: Target, test: RealTest) -> RunOutcome:  # pragma: no cover - protocol
        ...


@dataclasses.dataclass(frozen=True)
class PwshRunner:
    """The production runner: ``pwsh run.ps1`` on a Windows machine.

    One test is one ``run.ps1`` invocation, so a failure in one test cannot affect
    another and every test gets its own JUnit file.
    """

    artifacts: Path = HARNESS_DIR / "artifacts"
    script: Path = RUN_PS1
    pwsh: str = "pwsh"

    def command(self, target: Target, test: RealTest) -> list[str]:
        """The exact argument vector used to run one test (tested offline)."""
        return [
            self.pwsh,
            "-NoProfile",
            "-NonInteractive",
            "-File",
            str(self.script),
            "-Target",
            target.name,
            "-Test",
            test.name,
            "-Artifacts",
            str(self.artifacts),
            "-Json",
        ]

    def run(self, target: Target, test: RealTest) -> RunOutcome:
        """Run one test through ``run.ps1`` and read back its JSON outcome."""
        completed = subprocess.run(
            self.command(target, test),
            capture_output=True,
            text=True,
            check=False,
        )
        payload = _last_json_object(completed.stdout)
        if payload is None:
            return RunOutcome(
                target=target.name,
                test=test.name,
                status=STATUS_ERROR,
                message=(
                    "run.ps1 produced no JSON outcome"
                    f" (exit {completed.returncode}): {completed.stderr.strip()[:500]}"
                ),
            )
        junit = ""
        junit_path = payload.get("junit_path")
        if isinstance(junit_path, str) and junit_path:
            try:
                junit = Path(junit_path).read_text(encoding="utf-8")
            except OSError as exc:
                junit = ""
                payload.setdefault("message", f"cannot read {junit_path}: {exc}")
        return RunOutcome(
            target=target.name,
            test=test.name,
            status=str(payload.get("status", STATUS_ERROR)),
            passed=int(payload.get("passed", 0)),
            failed=int(payload.get("failed", 0)),
            skipped=int(payload.get("skipped", 0)),
            message=str(payload.get("message", "")),
            junit=junit,
        )


def _last_json_object(text: str) -> dict | None:
    """The last line of ``text`` that parses as a JSON object, or ``None``."""
    for line in reversed(text.splitlines()):
        line = line.strip()
        if not line.startswith("{"):
            continue
        try:
            parsed = json.loads(line)
        except json.JSONDecodeError:
            continue
        if isinstance(parsed, dict):
            return parsed
    return None


@dataclasses.dataclass(frozen=True)
class MatrixReport:
    """Every outcome of a matrix run, with the JUnit and text views over them."""

    outcomes: tuple[RunOutcome, ...]

    @property
    def ok(self) -> bool:
        """True when no outcome failed and none errored (skips are allowed)."""
        return all(outcome.status in (STATUS_PASS, STATUS_SKIP) for outcome in self.outcomes)

    def counts(self) -> dict[str, int]:
        counts = {status: 0 for status in STATUSES}
        for outcome in self.outcomes:
            counts[outcome.status] += 1
        return counts

    def render(self) -> str:
        """The printed matrix: one line per target/test pair, then a summary."""
        targets = sorted({outcome.target for outcome in self.outcomes})
        header = ("TARGET", "TEST", "STATUS", "DETAIL")
        rows = [
            (
                outcome.target,
                outcome.test,
                outcome.status,
                outcome.message or _detail(outcome),
            )
            for outcome in self.outcomes
        ]
        widths = [
            max(len(header[column]), *(len(row[column]) for row in rows)) if rows else len(header[column])
            for column in range(len(header))
        ]
        lines = ["  ".join(header[c].ljust(widths[c]) for c in range(len(header))).rstrip()]
        lines.append("  ".join("-" * width for width in widths))
        for row in rows:
            lines.append("  ".join(row[c].ljust(widths[c]) for c in range(len(header))).rstrip())
        counts = self.counts()
        pairs = len(self.outcomes)
        summary = (
            f"{len(targets)} target(s) x {pairs} test run(s): "
            f"{counts[STATUS_PASS]} passed, {counts[STATUS_FAIL]} failed, "
            f"{counts[STATUS_ERROR]} errored, {counts[STATUS_SKIP]} skipped"
        )
        lines.append(summary)
        return "\n".join(lines)

    def to_junit(self) -> str:
        """One JUnit XML document over every outcome, grouped by target."""
        root = ET.Element("testsuites")
        by_target: dict[str, list[RunOutcome]] = {}
        for outcome in self.outcomes:
            by_target.setdefault(outcome.target, []).append(outcome)
        for target in sorted(by_target):
            suite = ET.SubElement(root, "testsuite", {"name": target})
            tests = failures = skipped = 0
            for outcome in by_target[target]:
                for case in _cases_for(outcome):
                    suite.append(case)
                    tests += 1
                    if case.find("failure") is not None or case.find("error") is not None:
                        failures += 1
                    elif case.find("skipped") is not None:
                        skipped += 1
            suite.set("tests", str(tests))
            suite.set("failures", str(failures))
            suite.set("skipped", str(skipped))
        ET.indent(root)
        return ET.tostring(root, encoding="unicode")


def _detail(outcome: RunOutcome) -> str:
    if outcome.status == STATUS_PASS:
        return f"{outcome.passed} passed"
    return ""


def _cases_for(outcome: RunOutcome) -> list[ET.Element]:
    """The ``testcase`` elements for one outcome.

    A runner that produced JUnit contributes its own cases; a skip or an error with no
    JUnit contributes one synthesised case so the aggregate never loses the pair.
    """
    if outcome.junit.strip():
        try:
            parsed = ET.fromstring(outcome.junit)
        except ET.ParseError:
            parsed = None
        if parsed is not None:
            cases = parsed.findall(".//testcase")
            if cases:
                return cases
    case = ET.Element("testcase", {"classname": outcome.target, "name": outcome.test})
    if outcome.status == STATUS_SKIP:
        skipped = ET.SubElement(case, "skipped")
        skipped.set("message", outcome.message or "capabilities missing")
    elif outcome.status in (STATUS_FAIL, STATUS_ERROR):
        failure = ET.SubElement(case, "failure")
        failure.set("message", outcome.message or outcome.status)
    return [case]


def run_matrix(
    matrix: Matrix,
    runner: Runner,
    *,
    targets: Iterable[str] | None = None,
    tests: Iterable[str] | None = None,
    on_outcome=None,
) -> MatrixReport:
    """Run every selected target/test pair whose capabilities hold.

    A pair whose target does not offer the test's capabilities is recorded as a skip and
    the runner is not called for it, so a machine that cannot run a test reports that
    honestly instead of a false pass.
    """
    selected_targets = matrix.select_targets(targets)
    selected_tests = matrix.select_tests(tests)
    outcomes: list[RunOutcome] = []
    for target in selected_targets:
        for test in selected_tests:
            missing = matrix.missing_capabilities(target, test)
            if missing:
                outcome = RunOutcome(
                    target=target.name,
                    test=test.name,
                    status=STATUS_SKIP,
                    message=f"missing capabilities: {', '.join(missing)}",
                )
            else:
                outcome = runner.run(target, test)
            outcomes.append(outcome)
            if on_outcome is not None:
                on_outcome(outcome)
    return MatrixReport(outcomes=tuple(outcomes))


def _parse_args(argv: Sequence[str] | None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    sub = parser.add_subparsers(dest="command", required=True)

    matrix = sub.add_parser("matrix", help="print the matrix definition")
    matrix.add_argument("--json", action="store_true", help="print JSON instead of text")

    run = sub.add_parser("run", help="run the matrix through run.ps1 on this machine")
    run.add_argument("--target", action="append", default=[], help="a target name (repeatable)")
    run.add_argument("--test", action="append", default=[], help="a test name (repeatable)")
    run.add_argument("--artifacts", type=Path, default=HARNESS_DIR / "artifacts")
    run.add_argument("--pwsh", default="pwsh", help="the PowerShell executable")

    return parser.parse_args(argv)


def _print_matrix(matrix: Matrix, as_json: bool) -> int:
    if as_json:
        print(json.dumps(matrix_to_dict(matrix), indent=2, sort_keys=True))
        return 0
    lines = [
        f"{matrix.wp} Windows module conformance matrix ({matrix.module})",
        "",
        f"targets ({len(matrix.targets)}):",
    ]
    for target in matrix.targets:
        lines.append(
            f"  {target.name}: {target.os} {target.version} [{target.edition}], "
            f"labels {','.join(target.runner_labels)}, "
            f"capabilities {','.join(target.capabilities)}"
        )
    lines.append("")
    lines.append(f"tests ({len(matrix.tests)}):")
    for test in matrix.tests:
        lines.append(f"  {test.name}: {test.file} (needs {','.join(test.capabilities)})")
    lines.append("")
    lines.append("capabilities:")
    for capability in matrix.capabilities:
        lines.append(f"  {capability.name}: {capability.description}")
    print("\n".join(lines))
    return 0


def main(argv: Sequence[str] | None = None) -> int:
    args = _parse_args(argv)
    try:
        matrix = load_matrix()
    except MatrixError as exc:
        print(f"orchestrator: {exc}", file=sys.stderr)
        return 2

    if args.command == "matrix":
        return _print_matrix(matrix, args.json)

    runner = PwshRunner(artifacts=args.artifacts, pwsh=args.pwsh)
    try:
        report = run_matrix(
            matrix,
            runner,
            targets=args.target,
            tests=args.test,
            on_outcome=lambda outcome: print(
                f"[{outcome.status}] {outcome.target}/{outcome.test} {outcome.message}".rstrip(),
                flush=True,
            ),
        )
    except MatrixError as exc:
        print(f"orchestrator: {exc}", file=sys.stderr)
        return 2
    print()
    print(report.render())
    return 0 if report.ok else 1


if __name__ == "__main__":
    sys.exit(main())
