"""Profile-level invariants the harness snapshot does not carry (WP-PRD-05.T2).

``checks.py`` judges the change feed, the outbox, sequences and documents. The shipped-host
profile adds three more promises about the recovered store, read from a second JSON document
(``ExtraSnapshot``) that the operator's reader prints:

- **No duplicate numbering.** No two documents of one series share a document number.
- **Balance invariants.** For every currency the debit total equals the credit total, in
  integer minor units (no floats; a trial balance that does not balance is a lost half of a
  posting).
- **Jobs are not double-run.** A job run key is recorded at most once, and a run that the
  store marks completed is not also marked pending.

A part the reader could not produce is listed in ``missing`` and reported as a violation; it is
never read as "fine".
"""

from __future__ import annotations

import json
from dataclasses import dataclass, field
from typing import Any

from checks import Invariant, Violation

NUMBERING = "profile-numbering-unique"
BALANCES = "profile-balances-hold"
JOBS = "profile-jobs-single-run"
COMPLETE = "profile-extras-complete"

PARTS = ("numbers", "ledger", "job_runs")


class ExtrasError(ValueError):
    """The extras JSON is malformed."""


@dataclass(frozen=True)
class ExtraSnapshot:
    """Numbering, ledger totals and job runs of the recovered store."""

    # series -> every document number issued in it (duplicates are the violation).
    numbers: dict[str, tuple[str, ...]] = field(default_factory=dict)
    # currency -> (debit minor units, credit minor units)
    ledger: dict[str, tuple[int, int]] = field(default_factory=dict)
    # job run key -> number of recorded runs
    job_runs: dict[str, int] = field(default_factory=dict)
    # job run key -> state ("pending" | "completed"), only where a key has both
    job_states: dict[str, tuple[str, ...]] = field(default_factory=dict)
    missing: tuple[str, ...] = ()

    @staticmethod
    def unavailable(reason_parts: tuple[str, ...] = PARTS) -> "ExtraSnapshot":
        """What a target that has no reader returns: every part missing, so the run fails."""
        return ExtraSnapshot(missing=tuple(reason_parts))

    @staticmethod
    def from_json(raw: dict[str, Any]) -> "ExtraSnapshot":
        try:
            ledger = {}
            for currency, pair in raw.get("ledger", {}).items():
                debit, credit = pair
                if isinstance(debit, bool) or isinstance(credit, bool):
                    raise TypeError("ledger totals are integers")
                if not isinstance(debit, int) or not isinstance(credit, int):
                    raise TypeError("ledger totals are integers (minor units), never floats")
                ledger[str(currency)] = (debit, credit)
            return ExtraSnapshot(
                numbers={str(k): tuple(str(n) for n in v) for k, v in raw.get("numbers", {}).items()},
                ledger=ledger,
                job_runs={str(k): int(v) for k, v in raw.get("job_runs", {}).items()},
                job_states={str(k): tuple(str(s) for s in v) for k, v in raw.get("job_states", {}).items()},
                missing=tuple(str(m) for m in raw.get("missing", ())),
            )
        except (TypeError, ValueError, AttributeError) as exc:
            raise ExtrasError(f"malformed extras snapshot: {exc}") from exc

    @staticmethod
    def from_json_text(text: str) -> "ExtraSnapshot":
        try:
            raw = json.loads(text)
        except json.JSONDecodeError as exc:
            raise ExtrasError(f"extras snapshot is not JSON: {exc}") from exc
        if not isinstance(raw, dict):
            raise ExtrasError("extras snapshot must be a JSON object")
        return ExtraSnapshot.from_json(raw)

    def to_json_text(self) -> str:
        return json.dumps(
            {
                "numbers": {k: list(v) for k, v in self.numbers.items()},
                "ledger": {k: list(v) for k, v in self.ledger.items()},
                "job_runs": self.job_runs,
                "job_states": {k: list(v) for k, v in self.job_states.items()},
                "missing": list(self.missing),
            },
            sort_keys=True,
        )


def numbering_violations(extras: ExtraSnapshot) -> list[Violation]:
    out = []
    for series, numbers in sorted(extras.numbers.items()):
        seen: set[str] = set()
        reported: set[str] = set()
        for number in numbers:
            if number in seen and number not in reported:
                reported.add(number)
                out.append(Violation(NUMBERING, "numbering", series, f"duplicate_number {number}"))
            seen.add(number)
    return out


def balance_violations(extras: ExtraSnapshot) -> list[Violation]:
    return [
        Violation(BALANCES, "ledger", currency, f"unbalanced debit={debit} credit={credit}")
        for currency, (debit, credit) in sorted(extras.ledger.items())
        if debit != credit
    ]


def job_violations(extras: ExtraSnapshot) -> list[Violation]:
    out = []
    for key, runs in sorted(extras.job_runs.items()):
        if runs > 1:
            out.append(Violation(JOBS, "job", key, f"double_run runs={runs}"))
    for key, states in sorted(extras.job_states.items()):
        if "completed" in states and "pending" in states:
            out.append(Violation(JOBS, "job", key, "completed_and_pending"))
    return out


def completeness_violations(extras: ExtraSnapshot) -> list[Violation]:
    return [Violation(COMPLETE, "extras", part, "part_unavailable") for part in extras.missing]


def invariants(extras: ExtraSnapshot) -> tuple[Invariant, ...]:
    """The extra invariants, bound to one cycle's extras, ready for ``check_all``."""
    return (
        Invariant(COMPLETE, "Every part of the profile snapshot could be read.", lambda _h, _s: completeness_violations(extras)),
        Invariant(NUMBERING, "No document number is issued twice in a series.", lambda _h, _s: numbering_violations(extras)),
        Invariant(BALANCES, "Debits equal credits per currency, in minor units.", lambda _h, _s: balance_violations(extras)),
        Invariant(JOBS, "A job run key is recorded once and not both pending and completed.", lambda _h, _s: job_violations(extras)),
    )
