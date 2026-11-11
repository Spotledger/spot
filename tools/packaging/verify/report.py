"""The machine-readable result of a run: one row per (distro, scenario).

The nightly job uploads this file as an artefact, so the shape is fixed and validated
before it is written: a stable ``schema_version``, a ``summary`` of counts and exit code, and
one row per scenario with its distro, status, duration in whole milliseconds and — for a
failure — the last 40 log lines.

No timestamp is recorded. The report describes the tree that was tested, not when.
"""

from __future__ import annotations

import json
from dataclasses import dataclass
from pathlib import Path

from runtime import LOG_LINES, tail

PASS = "pass"
FAIL = "fail"
SKIP = "skip"
ERROR = "error"

STATUSES = (PASS, FAIL, SKIP, ERROR)
#: The statuses that make the driver exit non-zero.
FAILING_STATUSES = (FAIL, ERROR)

SCHEMA_VERSION = 1
#: The distro value a scenario gets when it runs once for the whole run.
SHARED_DISTRO = "all"


@dataclass(frozen=True)
class ScenarioResult:
    """What one scenario did on one distro."""

    distro: str
    scenario: str
    status: str
    duration_ms: int
    detail: str = ""
    log: tuple[str, ...] = ()

    @property
    def failed(self) -> bool:
        """Whether this row makes the run fail."""
        return self.status in FAILING_STATUSES

    def row(self) -> dict:
        """The JSON shape of this row."""
        return {
            "distro": self.distro,
            "scenario": self.scenario,
            "status": self.status,
            "duration_ms": self.duration_ms,
            "detail": self.detail,
            "log": list(self.log[-LOG_LINES:]),
        }


def make(
    distro: str,
    scenario: str,
    status: str,
    duration_ms: int,
    detail: str = "",
    log: list[str] | tuple[str, ...] = (),
) -> ScenarioResult:
    """Build a result, keeping at most the last 40 log lines."""
    return ScenarioResult(
        distro=distro,
        scenario=scenario,
        status=status,
        duration_ms=max(0, int(duration_ms)),
        detail=detail,
        log=tail(list(log), LOG_LINES),
    )


def summarize(rows: list[ScenarioResult]) -> dict:
    """Counts per status, the number of rows and the exit code they imply."""
    counts = {status: 0 for status in STATUSES}
    for row in rows:
        counts[row.status] = counts.get(row.status, 0) + 1
    return {
        "total": len(rows),
        "pass": counts[PASS],
        "fail": counts[FAIL],
        "skip": counts[SKIP],
        "error": counts[ERROR],
        "exit_code": exit_code(rows),
    }


def exit_code(rows: list[ScenarioResult]) -> int:
    """``1`` when any row failed or errored, ``0`` when everything passed or skipped."""
    return 1 if any(row.failed for row in rows) else 0


def build(rows: list[ScenarioResult], distros: list[str]) -> dict:
    """The whole report document."""
    return {
        "schema_version": SCHEMA_VERSION,
        "distros": list(distros),
        "summary": summarize(rows),
        "scenarios": [row.row() for row in rows],
    }


def render(document: dict) -> str:
    """The report as JSON text: sorted keys, two-space indent, a trailing newline."""
    return json.dumps(document, indent=2, sort_keys=True) + "\n"


def write(path: Path, rows: list[ScenarioResult], distros: list[str]) -> Path:
    """Validate and write the report; returns the path written.

    Raises:
        ValueError: when the document does not satisfy :func:`validate`, so a broken report
            is never uploaded as if it were a result.
    """
    document = build(rows, distros)
    problems = validate(document)
    if problems:
        raise ValueError("refusing to write an invalid report: " + "; ".join(problems))
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(render(document), encoding="utf-8")
    return path


def validate(document: dict) -> list[str]:
    """The problems with ``document``, empty when it satisfies the schema."""
    problems: list[str] = []
    if not isinstance(document, dict):
        return ["the report is not a JSON object"]

    if document.get("schema_version") != SCHEMA_VERSION:
        problems.append(f"schema_version must be {SCHEMA_VERSION}")

    distros = document.get("distros")
    if not isinstance(distros, list) or not all(isinstance(item, str) for item in distros):
        problems.append("distros must be a list of strings")

    summary = document.get("summary")
    if not isinstance(summary, dict):
        problems.append("summary must be an object")
    else:
        for key in ("total", "pass", "fail", "skip", "error", "exit_code"):
            if not isinstance(summary.get(key), int) or isinstance(summary.get(key), bool):
                problems.append(f"summary.{key} must be an integer")

    rows = document.get("scenarios")
    if not isinstance(rows, list):
        problems.append("scenarios must be a list")
        return problems

    for index, row in enumerate(rows):
        where = f"scenarios[{index}]"
        if not isinstance(row, dict):
            problems.append(f"{where} must be an object")
            continue
        for key in ("distro", "scenario", "status", "detail"):
            if not isinstance(row.get(key), str):
                problems.append(f"{where}.{key} must be a string")
        if row.get("status") not in STATUSES:
            problems.append(f"{where}.status must be one of {', '.join(STATUSES)}")
        duration = row.get("duration_ms")
        if not isinstance(duration, int) or isinstance(duration, bool) or duration < 0:
            problems.append(f"{where}.duration_ms must be a non-negative integer")
        log = row.get("log")
        if not isinstance(log, list) or not all(isinstance(line, str) for line in log):
            problems.append(f"{where}.log must be a list of strings")
        elif len(log) > LOG_LINES:
            problems.append(f"{where}.log must hold at most {LOG_LINES} lines")
        if row.get("status") in FAILING_STATUSES and not row.get("detail"):
            problems.append(f"{where} failed without a detail message")

    if isinstance(summary, dict):
        expected = summarize_rows(rows)
        if summary.get("total") != len(rows):
            problems.append("summary.total does not match the number of rows")
        if summary.get("fail") != expected[FAIL] or summary.get("error") != expected[ERROR]:
            problems.append("summary counts do not match the rows")
    return problems


def summarize_rows(rows: list) -> dict:
    """Counts derived from raw JSON rows, so validation needs no dataclasses."""
    counts = {status: 0 for status in STATUSES}
    for row in rows:
        status = row.get("status")
        if status in counts:
            counts[status] += 1
    return counts


def render_table(rows: list[ScenarioResult]) -> str:
    """A human-readable table: one row per scenario, one line per distro summary."""
    if not rows:
        return "no scenario ran"
    width = max([len("scenario")] + [len(row.scenario) for row in rows]) + 2
    distro_width = max([len("distro")] + [len(row.distro) for row in rows]) + 2
    header = "scenario".ljust(width) + "distro".ljust(distro_width) + "status".ljust(8) + "duration"
    lines = [header, "-" * len(header)]
    for row in rows:
        lines.append(
            row.scenario.ljust(width)
            + row.distro.ljust(distro_width)
            + row.status.ljust(8)
            + f"{row.duration_ms // 1000}.{row.duration_ms % 1000:03d} s"
        )
    summary = summarize(rows)
    lines.append("")
    lines.append(
        f"{summary['total']} scenario(s): {summary['pass']} passed, {summary['fail']} failed, "
        f"{summary['error']} errored, {summary['skip']} skipped"
    )
    return "\n".join(lines)
