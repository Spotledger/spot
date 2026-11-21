"""The durability audit and the framework invariants, on hand-built histories and snapshots."""

from __future__ import annotations

import unittest

import support  # noqa: F401  (path bootstrap)

from checks import (
    DURABILITY_AUDIT,
    check_all,
    child_integrity,
    doc_version_continuity,
    durability_audit,
    link_integrity,
    missing_parts,
    outbox_unique,
    seq_store_consistent,
    unique_keys,
)
from l7history import COMMITTED, FAILED, INDETERMINATE, REJECTED, ROLLED_BACK
from storesnapshot import Change, ChildRow, Document, INSERT, OutboxEvent, StoreSnapshot, UPDATE
from support import created, entry, history


def details(violations) -> list[str]:
    return [v.detail for v in violations]


class DurabilityAuditTests(unittest.TestCase):
    def test_acknowledged_effect_present_passes(self) -> None:
        h = history(entry(0, 0, touched=(created("order", "a", 1),)))
        snap = StoreSnapshot(changes=(Change("order", "a", INSERT, 1),))
        self.assertEqual(durability_audit(h, snap), [])

    def test_acknowledged_effect_absent_is_reported(self) -> None:
        h = history(entry(0, 0, touched=(created("order", "a", 1),)))
        snap = StoreSnapshot()
        self.assertEqual(details(durability_audit(h, snap)), ["acknowledged_effect_absent operation=w0#0"])

    def test_unacknowledged_effect_present_is_reported(self) -> None:
        h = history(entry(0, 0, outcome={"outcome": ROLLED_BACK}, touched=(created("order", "a", 1),)))
        snap = StoreSnapshot(changes=(Change("order", "a", INSERT, 1),))
        self.assertIn("unacknowledged_effect_present operation=w0#0", details(durability_audit(h, snap)))

    def test_partial_transaction_is_reported(self) -> None:
        h = history(
            entry(
                0,
                0,
                outcome={"outcome": FAILED, "code": "io", "exhausted": False},
                touched=(created("order", "a", 1), created("order", "b", 1)),
            )
        )
        snap = StoreSnapshot(changes=(Change("order", "a", INSERT, 1),))
        self.assertIn("partial_transaction present=1 absent=1", details(durability_audit(h, snap)))

    def test_indeterminate_committed_is_judged_as_committed(self) -> None:
        h = history(
            entry(0, 0, outcome={"outcome": INDETERMINATE, "resolved": True}, touched=(created("order", "a", 1),))
        )
        self.assertEqual(durability_audit(h, StoreSnapshot(changes=(Change("order", "a", INSERT, 1),))), [])

    def test_unresolved_indeterminate_is_a_violation(self) -> None:
        h = history(entry(0, 0, outcome={"outcome": INDETERMINATE, "resolved": None}))
        self.assertIn("unresolved_indeterminate_commit", details(durability_audit(h, StoreSnapshot())))

    def test_sequence_duplicate_is_reported(self) -> None:
        kind = {"kind": "allocate_sequence", "series": 0, "rollback": False}
        h = history(
            entry(0, 0, kind=kind, values={"number": 1}),
            entry(0, 1, kind=kind, values={"number": 1}),
        )
        self.assertIn("duplicate_allocation number=1", details(durability_audit(h, StoreSnapshot())))

    def test_sequence_gap_is_reported(self) -> None:
        kind = {"kind": "allocate_sequence", "series": 0, "rollback": False}
        h = history(entry(0, 0, kind=kind, values={"number": 2}))
        self.assertIn("gap_in_committed_allocations missing=1 count=1", details(durability_audit(h, StoreSnapshot())))

    def test_rolled_back_allocation_does_not_count(self) -> None:
        kind = {"kind": "allocate_sequence", "series": 0, "rollback": True}
        h = history(
            entry(0, 0, outcome={"outcome": ROLLED_BACK}, kind=kind, values={"number": 1}),
            entry(0, 1, kind=kind, values={"number": 1}),
        )
        self.assertEqual(durability_audit(h, StoreSnapshot()), [])

    def test_duplicate_outbox_event_is_reported(self) -> None:
        h = history(entry(0, 0))
        snap = StoreSnapshot(outbox=(OutboxEvent("e1", "commit", "order", "a"), OutboxEvent("e1", "commit", "order", "a")))
        self.assertIn("duplicate_event copies=2", details(durability_audit(h, snap)))


class FrameworkInvariantTests(unittest.TestCase):
    def test_doc_version_continuity_ok(self) -> None:
        snap = StoreSnapshot(
            changes=(Change("order", "a", INSERT, 1), Change("order", "a", UPDATE, 2)),
            documents=(Document("order", "a", version=2),),
        )
        self.assertEqual(doc_version_continuity(history(), snap), [])

    def test_doc_version_continuity_lost_update(self) -> None:
        snap = StoreSnapshot(
            changes=(Change("order", "a", INSERT, 1),),
            documents=(Document("order", "a", version=5),),
        )
        self.assertIn("version expected=1 actual=5", details(doc_version_continuity(history(), snap)))

    def test_child_integrity_duplicate_row(self) -> None:
        row = ChildRow("lines", "r1", 1)
        snap = StoreSnapshot(
            documents=(
                Document("order", "a", version=1, children=(row,)),
                Document("order", "b", version=1, children=(row,)),
            )
        )
        self.assertIn("duplicate_child_row", details(child_integrity(history(), snap)))

    def test_child_integrity_negative_qty(self) -> None:
        snap = StoreSnapshot(documents=(Document("order", "a", version=1, children=(ChildRow("lines", "r", -1),)),))
        self.assertIn("negative_qty=-1", details(child_integrity(history(), snap)))

    def test_link_integrity_dangling(self) -> None:
        snap = StoreSnapshot(documents=(Document("order", "a", version=1, links=(("party_link", "missing"),)),))
        self.assertIn("dangling_link field=party_link", details(link_integrity(history(), snap)))

    def test_link_integrity_ok(self) -> None:
        snap = StoreSnapshot(
            documents=(
                Document("order", "a", version=1, links=(("party_link", "p1"),)),
                Document("party", "p1", version=1),
            )
        )
        self.assertEqual(link_integrity(history(), snap), [])

    def test_unique_keys_duplicate_name(self) -> None:
        snap = StoreSnapshot(
            documents=(Document("party", "a", version=1, name="x"), Document("party", "b", version=1, name="x"))
        )
        self.assertIn("duplicate_name=x", details(unique_keys(history(), snap)))

    def test_seq_store_consistent(self) -> None:
        kind = {"kind": "allocate_sequence", "series": 0, "rollback": False}
        h = history(entry(0, 0, kind=kind, values={"number": 1}))
        self.assertEqual(seq_store_consistent(h, StoreSnapshot(sequences={"0": 2})), [])
        self.assertIn(
            "next_number=1 below_committed_max_plus_one=2",
            details(seq_store_consistent(h, StoreSnapshot(sequences={"0": 1}))),
        )

    def test_outbox_unique(self) -> None:
        snap = StoreSnapshot(outbox=(OutboxEvent("e1", "t", "order", "a"), OutboxEvent("e1", "t", "order", "a")))
        self.assertEqual(len(outbox_unique(history(), snap)), 1)

    def test_missing_parts(self) -> None:
        snap = StoreSnapshot(missing=("changes",))
        self.assertEqual(details(missing_parts(snap)), ["part_unavailable"])


class ReportTests(unittest.TestCase):
    def test_ok_report(self) -> None:
        report = check_all(history(entry(0, 0)), StoreSnapshot(), seed=3)
        self.assertTrue(report.ok)
        self.assertEqual(report.failed_ids(), [])
        self.assertIn("powerloss invariants: OK", report.to_text())
        self.assertIn('"schema": "spot-powerloss-report/1"', report.to_json())

    def test_failing_report_names_the_invariant(self) -> None:
        h = history(entry(0, 0, touched=(created("order", "a", 1),)))
        report = check_all(h, StoreSnapshot(), seed=3)
        self.assertFalse(report.ok)
        self.assertIn(DURABILITY_AUDIT, report.failed_ids())
        self.assertTrue(report.violations_of(DURABILITY_AUDIT))

    def test_violations_are_capped(self) -> None:
        h = history(*(entry(0, i, touched=(created("order", f"o{i}", 1),)) for i in range(500)))
        report = check_all(h, StoreSnapshot(), seed=3)
        result = report.results[0]
        self.assertEqual(result.total_violations, 500)
        self.assertEqual(len(result.violations), 200)


if __name__ == "__main__":
    unittest.main()
