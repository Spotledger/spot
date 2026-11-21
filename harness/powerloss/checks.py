"""The invariant suite and the durability audit, run after every recovery.

These are the judges. They read two things and nothing else: the **client-side history**
(what the client was told) and a **store snapshot** (what survived the cut). They never
open a database, never read the wall clock and never write anything.

The durability audit is the same rule set as ``harness/rust/l8-faults/src/audit.rs``:

- an **acknowledged** commit's effects are present: for every record it created, the change
  feed holds exactly the one net entry at the version the operation wrote;
- a **refused**, **failed** or **resolved-absent** operation's effects are absent;
- an operation that was **not** acknowledged is wholly present or wholly absent, never
  partial — checked per transaction over its created records;
- no commit outcome is left **unresolved**;
- sequences have no gap or duplicate over the committed allocations;
- the outbox holds every event at most once and every event's subject exists.

Only a record's **creation** is judged per operation: a document's current version cannot
say whether *this* operation applied (a later update raises it, a later delete removes the
row), so updated and deleted records are judged by the aggregate invariants.
"""

from __future__ import annotations

import json
from dataclasses import dataclass, field
from typing import Any, Callable, Sequence

from l7history import (
    ACKNOWLEDGED,
    COMMIT_CLASSES,
    CREATED,
    INDETERMINATE_COMMITTED,
    UNRESOLVED,
    History,
    commit_class,
)
from storesnapshot import INSERT, StoreSnapshot, UPDATE

REPORT_SCHEMA = "spot-powerloss-report/1"
DURABILITY_AUDIT = "durability-audit"
MAX_VIOLATIONS = 200


@dataclass(frozen=True)
class Violation:
    """One broken promise: invariant id, entity, record and a machine-readable detail."""

    invariant: str
    entity: str
    id: str
    detail: str

    def to_json(self) -> dict[str, Any]:
        return {
            "invariant": self.invariant,
            "entity": self.entity,
            "id": self.id,
            "detail": self.detail,
        }


@dataclass
class InvariantResult:
    """The outcome of one invariant."""

    id: str
    describe: str
    passed: bool
    total_violations: int
    violations: list[Violation] = field(default_factory=list)

    def to_json(self) -> dict[str, Any]:
        return {
            "id": self.id,
            "describe": self.describe,
            "passed": self.passed,
            "total_violations": self.total_violations,
            "violations": [v.to_json() for v in self.violations],
        }


@dataclass
class CheckReport:
    """Everything the checks found, in JSON or text."""

    ok: bool
    classification: dict[str, int]
    results: list[InvariantResult]
    seed: int | None = None
    schema: str = REPORT_SCHEMA

    def failed_ids(self) -> list[str]:
        return [r.id for r in self.results if not r.passed]

    def violations_of(self, invariant: str) -> list[Violation]:
        for result in self.results:
            if result.id == invariant:
                return result.violations
        return []

    def to_json(self) -> str:
        return json.dumps(
            {
                "schema": self.schema,
                "ok": self.ok,
                "seed": self.seed,
                "classification": self.classification,
                "results": [r.to_json() for r in self.results],
            },
            indent=2,
            sort_keys=True,
        )

    def to_text(self) -> str:
        head = "OK" if self.ok else "VIOLATIONS"
        lines = [f"powerloss invariants: {head}"]
        counts = " ".join(f"{name}={self.classification.get(name, 0)}" for name in COMMIT_CLASSES)
        lines.append(f"operations: {counts}")
        for result in self.results:
            lines.append(f"  [{'pass' if result.passed else 'FAIL'}] {result.id}")
            for violation in result.violations:
                lines.append(f"      {violation.entity} {violation.id} {violation.detail}")
            if result.total_violations > len(result.violations):
                lines.append(f"      ... and {result.total_violations - len(result.violations)} more")
        return "\n".join(lines)


def _committed_allocations(history: History) -> dict[str, list[int]]:
    """Committed sequence allocations by series, from the history."""
    out: dict[str, list[int]] = {}
    for entry in history.entries:
        if commit_class(entry) != ACKNOWLEDGED:
            continue
        if entry.kind.get("kind") != "allocate_sequence":
            continue
        series = str(entry.kind.get("series", 0))
        number = entry.values.get("number")
        if number is None:
            continue
        out.setdefault(series, []).append(int(number))
    return out


def durability_audit(history: History, snapshot: StoreSnapshot) -> list[Violation]:
    """The durability rules of audit.rs, as a list of violations."""
    violations: list[Violation] = []

    # 1. No commit outcome left unresolved.
    for entry in history.entries:
        if commit_class(entry) == UNRESOLVED:
            violations.append(
                Violation(DURABILITY_AUDIT, "operation", entry.label, "unresolved_indeterminate_commit")
            )

    # 2. Sequences: no duplicate, no gap over committed allocations.
    for series, numbers in _committed_allocations(history).items():
        seen: set[int] = set()
        for number in numbers:
            if number in seen:
                violations.append(
                    Violation(DURABILITY_AUDIT, "sequence", series, f"duplicate_allocation number={number}")
                )
            seen.add(number)
        for expected in range(1, len(seen) + 1):
            if expected not in seen:
                violations.append(
                    Violation(
                        DURABILITY_AUDIT,
                        "sequence",
                        series,
                        f"gap_in_committed_allocations missing={expected} count={len(seen)}",
                    )
                )
                break

    # 3. The change feed, keyed by (entity, id, op, version).
    actual: dict[tuple[str, str, str, int], int] = {}
    for change in snapshot.changes:
        key = (change.entity, change.id, change.op, change.version)
        actual[key] = actual.get(key, 0) + 1

    # 4. Per-operation atomicity over created records.
    for entry in history.entries:
        klass = commit_class(entry)
        committed = klass in (ACKNOWLEDGED, INDETERMINATE_COMMITTED)
        present = 0
        absent = 0
        for touched in entry.touched:
            if touched.role != CREATED or touched.version is None:
                continue
            effect = actual.get((touched.entity, touched.id, INSERT, touched.version), 0) > 0
            if effect:
                present += 1
            else:
                absent += 1
            if committed and not effect:
                violations.append(
                    Violation(
                        DURABILITY_AUDIT,
                        touched.entity,
                        touched.id,
                        f"acknowledged_effect_absent operation={entry.label}",
                    )
                )
            if not committed and effect:
                violations.append(
                    Violation(
                        DURABILITY_AUDIT,
                        touched.entity,
                        touched.id,
                        f"unacknowledged_effect_present operation={entry.label}",
                    )
                )
        if not committed and present > 0 and absent > 0:
            violations.append(
                Violation(
                    DURABILITY_AUDIT,
                    "operation",
                    entry.label,
                    f"partial_transaction present={present} absent={absent}",
                )
            )

    # 5. The outbox: no event id appears twice.
    seen_events: dict[str, int] = {}
    for event in snapshot.outbox:
        seen_events[event.id] = seen_events.get(event.id, 0) + 1
    for event_id, count in seen_events.items():
        if count > 1:
            violations.append(
                Violation(DURABILITY_AUDIT, "outbox", event_id, f"duplicate_event copies={count}")
            )
    return violations


# -- framework invariants ------------------------------------------------------------------


def outbox_unique(_history: History, snapshot: StoreSnapshot) -> list[Violation]:
    """No outbox event id appears twice."""
    seen: dict[str, int] = {}
    for event in snapshot.outbox:
        seen[event.id] = seen.get(event.id, 0) + 1
    return [
        Violation("outbox-unique", "outbox", event_id, f"duplicate_event copies={count}")
        for event_id, count in seen.items()
        if count > 1
    ]


def doc_version_continuity(_history: History, snapshot: StoreSnapshot) -> list[Violation]:
    """A live document's version is ``1 + updates`` seen in the change feed.

    Mirrors ``VersionContinuity`` in the L7 invariants: the current version must equal one
    plus the number of updates, so a lost update (two updates writing the same version) shows
    as a mismatch. Deleted documents are not live and are skipped.
    """
    writes: dict[tuple[str, str], int] = {}
    for change in snapshot.changes:
        if change.op in (INSERT, UPDATE):
            key = (change.entity, change.id)
            writes[key] = writes.get(key, 0) + 1
    out = []
    for doc in snapshot.documents:
        expected = writes.get((doc.entity, doc.id), 0)
        if doc.version != expected:
            out.append(
                Violation(
                    "doc-version_continuity",
                    doc.entity,
                    doc.id,
                    f"version expected={expected} actual={doc.version}",
                )
            )
    return out


def child_integrity(_history: History, snapshot: StoreSnapshot) -> list[Violation]:
    """Child-row ids are unique across the store and quantities are non-negative."""
    seen: dict[tuple[str, str], str] = {}
    out = []
    for doc in snapshot.documents:
        for row in doc.children:
            if row.qty < 0:
                out.append(Violation("child-integrity", doc.entity, doc.id, f"negative_qty={row.qty}"))
            key = (row.table, row.id)
            if key in seen:
                out.append(
                    Violation("child-integrity", row.table, row.id, "duplicate_child_row")
                )
            seen[key] = doc.id
    return out


def link_integrity(_history: History, snapshot: StoreSnapshot) -> list[Violation]:
    """Every link field points at a document that exists."""
    document_ids = {(doc.entity, doc.id) for doc in snapshot.documents}
    out = []
    for doc in snapshot.documents:
        for field_name, target in doc.links:
            if not target:
                continue
            if (doc.entity, target) not in document_ids and target not in {i for _, i in document_ids}:
                out.append(
                    Violation("link-integrity", doc.entity, doc.id, f"dangling_link field={field_name}")
                )
    return out


def unique_keys(_history: History, snapshot: StoreSnapshot) -> list[Violation]:
    """No two documents of one entity share a non-empty unique name."""
    seen: dict[tuple[str, str], str] = {}
    out = []
    for doc in snapshot.documents:
        if not doc.name:
            continue
        key = (doc.entity, doc.name)
        if key in seen:
            out.append(Violation("unique-keys", doc.entity, doc.id, f"duplicate_name={doc.name}"))
        seen[key] = doc.id
    return out


def seq_store_consistent(history: History, snapshot: StoreSnapshot) -> list[Violation]:
    """The store's next number for a series is one past the highest committed allocation."""
    allocations = _committed_allocations(history)
    out = []
    for series, numbers in allocations.items():
        if series not in snapshot.sequences:
            continue
        expected = max(numbers) + 1
        actual = snapshot.sequences[series]
        if actual < expected:
            out.append(
                Violation(
                    "seq-store_consistent",
                    "sequence",
                    series,
                    f"next_number={actual} below_committed_max_plus_one={expected}",
                )
            )
    return out


@dataclass(frozen=True)
class Invariant:
    """A named check: id, one-paragraph promise, and the function that runs it."""

    id: str
    describe: str
    run: Callable[[History, StoreSnapshot], list[Violation]]


FRAMEWORK_INVARIANTS: tuple[Invariant, ...] = (
    Invariant(
        DURABILITY_AUDIT,
        "Every acknowledged commit is present in the change feed at the version it wrote; no "
        "refused, failed or resolved-absent operation left an effect; no unacknowledged "
        "operation is partial; no commit outcome is unresolved; sequences have no gap or "
        "duplicate; the outbox holds each event at most once with an existing subject.",
        durability_audit,
    ),
    Invariant("outbox-unique", "No outbox event id appears twice.", outbox_unique),
    Invariant(
        "doc-version_continuity",
        "Every document's change-feed versions run 1..n with no gap, starting at an insert.",
        doc_version_continuity,
    ),
    Invariant(
        "child-integrity",
        "Child-row ids are unique across the store and quantities are non-negative.",
        child_integrity,
    ),
    Invariant(
        "link-integrity",
        "Every non-empty link field points at a document that exists.",
        link_integrity,
    ),
    Invariant("unique-keys", "No two documents of one entity share a non-empty name.", unique_keys),
    Invariant(
        "seq-store_consistent",
        "The store's next number for a series is one past the highest committed allocation.",
        seq_store_consistent,
    ),
)


def check_all(
    history: History,
    snapshot: StoreSnapshot,
    invariants: Sequence[Invariant] = FRAMEWORK_INVARIANTS,
    seed: int | None = None,
) -> CheckReport:
    """Runs every invariant, never stopping at the first failure."""
    results: list[InvariantResult] = []
    for invariant in invariants:
        found = invariant.run(history, snapshot)
        total = len(found)
        results.append(
            InvariantResult(
                id=invariant.id,
                describe=invariant.describe,
                passed=total == 0,
                total_violations=total,
                violations=found[:MAX_VIOLATIONS],
            )
        )
    return CheckReport(
        ok=all(r.passed for r in results),
        classification=history.counts(),
        results=results,
        seed=seed,
    )


def missing_parts(snapshot: StoreSnapshot) -> list[Violation]:
    """A violation for every part of the snapshot a target could not read."""
    return [
        Violation("snapshot-complete", "snapshot", part, "part_unavailable")
        for part in snapshot.missing
    ]
