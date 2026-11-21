"""The model store: the durability switches, recovery and the sequence rollback."""

from __future__ import annotations

import unittest

import support  # noqa: F401  (path bootstrap)

from fakestore import FakeStore, StoreError
from storesnapshot import OutboxEvent


class DurabilityTests(unittest.TestCase):
    def _insert(self, store: FakeStore, id: str = "a") -> None:
        tx = store.begin()
        tx.insert("order", id, {"total": 1})
        tx.commit()

    def test_durable_commit_survives_a_cut(self) -> None:
        store = FakeStore(fsync=True, synchronous_commit=True)
        self._insert(store)
        store.cut()
        self.assertIsNotNone(store.document("order", "a"))

    def test_fsync_off_loses_the_commit(self) -> None:
        store = FakeStore(fsync=False, synchronous_commit=True)
        self._insert(store)
        self.assertIsNotNone(store.document("order", "a"))  # visible before the cut
        store.cut()
        self.assertIsNone(store.document("order", "a"))

    def test_synchronous_commit_off_loses_the_commit(self) -> None:
        store = FakeStore(fsync=True, synchronous_commit=False)
        self._insert(store)
        store.cut()
        self.assertIsNone(store.document("order", "a"))

    def test_earlier_durable_commits_survive_a_later_unsynced_one(self) -> None:
        store = FakeStore(fsync=True, synchronous_commit=False)
        # A checkpoint-like sync of the first commit.
        self._insert(store, "a")
        store._synced = len(store._wal)  # noqa: SLF001 - test reaches into the model on purpose
        self._insert(store, "b")
        store.cut()
        self.assertIsNotNone(store.document("order", "a"))
        self.assertIsNone(store.document("order", "b"))

    def test_settings_are_reported(self) -> None:
        store = FakeStore(fsync=False, synchronous_commit=True, full_page_writes=True)
        self.assertEqual(
            store.settings(),
            {"fsync": "off", "synchronous_commit": "on", "full_page_writes": "on"},
        )


class TransactionTests(unittest.TestCase):
    def test_version_conflict_is_refused(self) -> None:
        store = FakeStore()
        tx = store.begin()
        tx.insert("order", "a", {"total": 1})
        tx.commit()
        tx = store.begin()
        with self.assertRaises(StoreError):
            tx.update("order", "a", 99, {"total": 2})
        tx.rollback()

    def test_rollback_releases_an_allocation(self) -> None:
        store = FakeStore()
        tx = store.begin()
        self.assertEqual(tx.allocate("s", 1), 1)
        tx.rollback()
        tx = store.begin()
        self.assertEqual(tx.allocate("s", 1), 1)  # the number is available again
        tx.commit()

    def test_commit_advances_the_series(self) -> None:
        store = FakeStore()
        tx = store.begin()
        self.assertEqual(tx.allocate("s", 1), 1)
        tx.commit()
        tx = store.begin()
        self.assertEqual(tx.allocate("s", 1), 2)
        tx.commit()
        self.assertEqual(store.snapshot().sequences["s"], 3)

    def test_delete_then_rollback_keeps_the_document(self) -> None:
        store = FakeStore()
        tx = store.begin()
        tx.insert("order", "a", {"total": 1})
        tx.commit()
        tx = store.begin()
        tx.delete("order", "a", 1)
        tx.rollback()
        self.assertIsNotNone(store.document("order", "a"))

    def test_outbox_events_are_recorded(self) -> None:
        store = FakeStore()
        tx = store.begin()
        tx.enqueue(OutboxEvent("e1", "commit", "order", "a"))
        tx.commit()
        self.assertEqual([e.id for e in store.snapshot().outbox], ["e1"])


if __name__ == "__main__":
    unittest.main()
