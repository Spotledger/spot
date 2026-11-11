"""The conformance matrix model (WP-STR-07.T1).

A *matrix* is one row per language. A row is either ``SUPPORTED`` (the component built here and
every contract case passed), ``BLOCKED`` (a named step stopped it, with a reason), ``FAILED`` (it
built and ran but a contract case disagreed) or ``NOT_RUN`` (``run.py --list`` never builds).

The whole point of the work package is honesty: a language is supported only when a conformance
run demonstrated it, and a blocked row must name the step and the reason. This module is pure
data plus formatting, so the unit tests need no containers.
"""

from __future__ import annotations

import json
from dataclasses import dataclass, field
from typing import Any

SUPPORTED = "SUPPORTED"
BLOCKED = "BLOCKED"
FAILED = "FAILED"
NOT_RUN = "NOT_RUN"

ALL_STATUSES = (SUPPORTED, BLOCKED, FAILED, NOT_RUN)

#: The language that is always supported by construction: the host and its world fixtures are
#: Rust/WebAssembly, so the reference row must never come back blocked or failed.
REFERENCE_LANGUAGE = "rust"


@dataclass
class Blocked:
    """Why a language could not be run: the step that stopped and the reason."""

    step: str
    reason: str

    def render(self) -> str:
        """``BLOCKED(<step>, <reason>)``, the form the brief asks the runner to record."""
        return f"BLOCKED({self.step}, {self.reason})"

    def to_dict(self) -> dict[str, str]:
        """A JSON object."""
        return {"step": self.step, "reason": self.reason}

    @classmethod
    def from_dict(cls, data: dict[str, Any]) -> "Blocked":
        """Reads ``{"step": ..., "reason": ...}``."""
        return cls(step=str(data.get("step", "")), reason=str(data.get("reason", "")))


@dataclass
class Outcome:
    """One language's conformance outcome."""

    language: str
    status: str
    image: str = ""
    toolchain: str = ""
    binary_bytes: int | None = None
    component_sha256: str | None = None
    imports: list[str] = field(default_factory=list)
    cases: list[dict[str, Any]] = field(default_factory=list)
    blocked: Blocked | None = None
    notes: list[str] = field(default_factory=list)

    def __post_init__(self) -> None:
        if self.status not in ALL_STATUSES:
            raise ValueError(f"unknown status {self.status!r}; expected one of {ALL_STATUSES}")

    @property
    def cases_passed(self) -> int:
        """How many cases passed."""
        return sum(1 for case in self.cases if case.get("passed"))

    @property
    def cases_total(self) -> int:
        """How many cases ran."""
        return len(self.cases)

    def render_status(self) -> str:
        """The status as the brief spells it: a blocked row shows ``BLOCKED(step, reason)``."""
        if self.status == BLOCKED:
            if self.blocked is None:
                return f"{BLOCKED}(<unknown>, <no reason>)"
            return self.blocked.render()
        return self.status

    def to_dict(self) -> dict[str, Any]:
        """A JSON object."""
        return {
            "language": self.language,
            "status": self.status,
            "image": self.image,
            "toolchain": self.toolchain,
            "binary_bytes": self.binary_bytes,
            "component_sha256": self.component_sha256,
            "imports": list(self.imports),
            "cases": list(self.cases),
            "blocked": self.blocked.to_dict() if self.blocked else None,
            "notes": list(self.notes),
        }

    @classmethod
    def from_dict(cls, data: dict[str, Any]) -> "Outcome":
        """Reads the JSON object :meth:`to_dict` writes."""
        blocked = data.get("blocked")
        return cls(
            language=str(data["language"]),
            status=str(data["status"]),
            image=str(data.get("image", "")),
            toolchain=str(data.get("toolchain", "")),
            binary_bytes=data.get("binary_bytes"),
            component_sha256=data.get("component_sha256"),
            imports=list(data.get("imports", [])),
            cases=list(data.get("cases", [])),
            blocked=Blocked.from_dict(blocked) if blocked else None,
            notes=list(data.get("notes", [])),
        )


def blocked(language: str, step: str, reason: str, **kwargs: Any) -> Outcome:
    """A blocked row, for the short call sites in ``run.py``."""
    return Outcome(
        language=language,
        status=BLOCKED,
        blocked=Blocked(step=step, reason=reason),
        **kwargs,
    )


def parse_status(text: str) -> tuple[str, Blocked | None]:
    """Parses a rendered status back into ``(status, blocked)``.

    Accepts ``SUPPORTED``, ``FAILED``, ``NOT_RUN`` and ``BLOCKED(<step>, <reason>)``. Raises
    :class:`ValueError` for anything else, so a typo in a stored matrix is caught.
    """
    stripped = text.strip()
    if stripped in (SUPPORTED, FAILED, NOT_RUN):
        return stripped, None
    if stripped.startswith(f"{BLOCKED}(") and stripped.endswith(")"):
        inner = stripped[len(BLOCKED) + 1 : -1]
        step, _, reason = inner.partition(",")
        step, reason = step.strip(), reason.strip()
        if not step or not reason:
            raise ValueError(f"a blocked status needs a step and a reason: {text!r}")
        return BLOCKED, Blocked(step=step, reason=reason)
    raise ValueError(f"not a status: {text!r}")


def overall(outcomes: list[Outcome]) -> tuple[bool, list[str]]:
    """Whether the matrix passes, and the problems found.

    It passes when:

    - the reference language (:data:`REFERENCE_LANGUAGE`) is present and ``SUPPORTED``;
    - every ``SUPPORTED`` row has at least one case and every case passed;
    - every ``BLOCKED`` row names a non-empty step and reason;
    - no row is ``FAILED``.

    A ``NOT_RUN`` row is allowed: it only means the harness was asked to list, not to build.
    """
    problems: list[str] = []
    by_language = {o.language: o for o in outcomes}

    reference = by_language.get(REFERENCE_LANGUAGE)
    if reference is None:
        problems.append(f"the reference language {REFERENCE_LANGUAGE!r} is missing from the matrix")
    elif reference.status != SUPPORTED:
        problems.append(
            f"the reference language {REFERENCE_LANGUAGE!r} must be SUPPORTED, not "
            f"{reference.render_status()}"
        )

    for outcome in outcomes:
        if outcome.status == SUPPORTED:
            if not outcome.cases:
                problems.append(f"{outcome.language}: SUPPORTED with no contract cases run")
            else:
                failed = [c.get("name", "?") for c in outcome.cases if not c.get("passed")]
                if failed:
                    problems.append(
                        f"{outcome.language}: SUPPORTED but cases failed: {', '.join(failed)}"
                    )
        elif outcome.status == BLOCKED:
            if outcome.blocked is None or not outcome.blocked.step or not outcome.blocked.reason:
                problems.append(f"{outcome.language}: BLOCKED without a step and a reason")
        elif outcome.status == FAILED:
            problems.append(f"{outcome.language}: FAILED a contract case")
    return (not problems), problems


def to_json(outcomes: list[Outcome]) -> str:
    """The whole matrix as a JSON document."""
    ok, problems = overall(outcomes)
    return json.dumps(
        {
            "passed": ok,
            "problems": problems,
            "languages": [o.to_dict() for o in outcomes],
        },
        indent=2,
        sort_keys=True,
    )


def from_json(text: str) -> list[Outcome]:
    """Reads the JSON document :func:`to_json` writes."""
    data = json.loads(text)
    return [Outcome.from_dict(row) for row in data["languages"]]


def _cell(text: str) -> str:
    """Escapes the table cell separators Markdown would otherwise read."""
    return text.replace("|", "\\|").replace("\n", " ")


def markdown_table(outcomes: list[Outcome]) -> str:
    """The matrix as a Markdown table, one row per language."""
    lines = [
        "| Language | Status | Image | Component (bytes) | Cases | Notes |",
        "| --- | --- | --- | --- | --- | --- |",
    ]
    for outcome in outcomes:
        size = "—" if outcome.binary_bytes is None else f"{outcome.binary_bytes:,}"
        if outcome.cases_total:
            cases = f"{outcome.cases_passed}/{outcome.cases_total}"
        elif outcome.status == NOT_RUN:
            cases = "—"
        else:
            cases = "0/0"
        notes = "; ".join(outcome.notes)
        lines.append(
            "| "
            + " | ".join(
                [
                    _cell(outcome.language),
                    _cell(outcome.render_status()),
                    _cell(outcome.image or "—"),
                    size,
                    cases,
                    _cell(notes) or "—",
                ]
            )
            + " |"
        )
    return "\n".join(lines)
