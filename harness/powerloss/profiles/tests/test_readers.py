"""The post client, the snapshot reader and the extras reader (WP-PRD-05.T3), over a fake API."""

from __future__ import annotations

import os
import tempfile
import unittest

import profiles_support  # noqa: F401

import checks
import extras_reader
import fake_api
import l7history as h
import operator_http as oh
import post_client
import postings_audit
import shipped_extras
import store_reader
from workload import Rng

CFG = oh.Config("http://fake.invalid", "tester", "pw")
SECRET = "pw"


def _clock():
    it = iter(range(0, 10**9, 10))
    return lambda: next(it)


class Base(unittest.TestCase):
    def setUp(self) -> None:
        self.dir = tempfile.mkdtemp()
        self.addCleanup(lambda: __import__("shutil").rmtree(self.dir, ignore_errors=True))
        self.path = os.path.join(self.dir, "h.jsonl")
        self.api = fake_api.FakeApi(page=2)

    def post(self, n: int, cut: int = 0):
        return post_client.run(CFG, self.api.transport, 7, self.path, cut, n, _clock())

    def snap(self):
        return store_reader.read_snapshot(CFG, self.api.transport, ["outbox", "sequences"])


class PostClientTests(Base):
    def test_acknowledgements_are_recorded_with_status_and_name(self) -> None:
        self.assertEqual(self.post(4)["committed"], 4)
        entries = h.read_history(self.path).entries
        self.assertEqual(len(entries), 4)
        for e in entries:
            self.assertEqual(e.outcome["status"], 201)
            self.assertTrue(e.outcome["name"].startswith("POST-"))
            self.assertEqual(h.commit_class(e), h.ACKNOWLEDGED)

    def test_uuid7_shape_and_determinism(self) -> None:
        a, b = post_client.uuid7(Rng(1), 5), post_client.uuid7(Rng(1), 5)
        self.assertEqual(a, b)
        self.assertEqual(a[14], "7")
        self.assertIn(a[19], "89ab")

    def test_a_lost_reply_is_retried_with_the_same_key_and_not_duplicated(self) -> None:
        self.api.lose_reply_for.add("pl-7-1")
        self.assertEqual(self.post(3)["committed"], 3)
        self.assertEqual(len(self.api.docs), 3)
        self.assertEqual(h.read_history(self.path).entries[1].attempts, 2)

    def test_cut_after_stops_the_run(self) -> None:
        self.assertEqual(self.post(10, cut=3)["operations"], 3)

    def test_a_refusal_is_rejected_not_committed(self) -> None:
        bad = oh.Config("http://fake.invalid", "tester", "wrong")
        with self.assertRaises(oh.HttpError):
            post_client.run(bad, self.api.transport, 7, self.path, 0, 1, _clock())

    def test_the_password_is_never_in_the_config_text(self) -> None:
        self.assertNotIn(SECRET, repr(CFG))


class ReaderTests(Base):
    def test_documents_come_through_the_list_port_in_stable_order_across_pages(self) -> None:
        self.post(5)
        snap = self.snap()
        ids = [d.id for d in snap.documents]
        self.assertEqual(ids, sorted(ids))
        self.assertEqual(len(ids), 5)
        self.assertEqual(snap.missing, ())

    def test_unreadable_parts_are_missing_unless_declared_empty(self) -> None:
        self.post(1)
        bare = store_reader.read_snapshot(CFG, self.api.transport)
        self.assertEqual(set(bare.missing), {"outbox", "sequences"})
        with self.assertRaises(ValueError):
            store_reader.read_snapshot(CFG, self.api.transport, ["documents"])

    def test_an_acknowledged_posting_missing_after_restart_is_reported(self) -> None:
        self.post(4)
        history = h.read_history(self.path)
        del self.api.docs[next(iter(self.api.docs))]
        report = checks.check_all(history, self.snap())
        self.assertFalse(report.ok)
        self.assertIn(checks.DURABILITY_AUDIT, report.failed_ids())

    def test_an_unacknowledged_posting_that_appears_is_allowed_and_counted(self) -> None:
        self.post(3)
        history = h.read_history(self.path)
        self.api.docs["zz"] = {"id": "zz", "version": 1, "name": "N"}
        snap = self.snap()
        self.assertTrue(checks.check_all(history, snap).ok)
        self.assertEqual(postings_audit.unacknowledged_present(history, snap), 1)


class ExtrasTests(Base):
    def read(self, mgmt, empty=("ledger", "job_runs")):
        return shipped_extras.ExtraSnapshot.from_json(extras_reader.read_extras(CFG, self.api.transport, mgmt, empty))

    def test_head_and_outbox_come_from_the_management_port(self) -> None:
        self.post(2)
        doc = extras_reader.read_extras(
            CFG, self.api.transport, lambda: {"head": {"generation": 1, "seq": 9}, "outbox_pending": 3}, ["ledger", "job_runs"]
        )
        self.assertEqual((doc["journal_head"], doc["outbox_count"], doc["missing"]), ([1, 9], 3, []))
        self.assertEqual(len(doc["numbers"]["posting"]), 2)

    def test_no_management_client_leaves_the_parts_missing(self) -> None:
        extras = self.read(lambda: None)
        self.assertEqual(set(extras.missing), {"journal_head", "outbox_count"})
        self.assertTrue(shipped_extras.completeness_violations(extras))

    def test_undeclared_ledger_and_jobs_are_missing(self) -> None:
        extras = self.read(lambda: {"head": {"generation": 1, "seq": 1}, "outbox_pending": 0}, ())
        self.assertEqual(set(extras.missing), {"ledger", "job_runs"})


if __name__ == "__main__":
    unittest.main()
