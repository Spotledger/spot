"""The history format: round-trip, version guard, classification and durable append."""

from __future__ import annotations

import os
import unittest

import support  # noqa: F401  (path bootstrap)

from l7history import (
    ACKNOWLEDGED,
    COMMITTED,
    FAILED,
    FAILED_CLASS,
    INDETERMINATE,
    INDETERMINATE_ABSENT,
    INDETERMINATE_COMMITTED,
    REFUSED,
    REJECTED,
    ROLLED_BACK,
    UNRESOLVED,
    Entry,
    Header,
    History,
    HistoryError,
    HistoryWriter,
    Touched,
    commit_class,
    read_history,
)
from support import created, entry, history


class CommitClassTests(unittest.TestCase):
    def test_committed_is_acknowledged(self) -> None:
        self.assertEqual(commit_class(entry()), ACKNOWLEDGED)

    def test_rollback_and_rejection_are_refused(self) -> None:
        self.assertEqual(commit_class(entry(outcome={"outcome": ROLLED_BACK})), REFUSED)
        self.assertEqual(commit_class(entry(outcome={"outcome": REJECTED, "code": "not_found"})), REFUSED)

    def test_failed_is_failed(self) -> None:
        self.assertEqual(
            commit_class(entry(outcome={"outcome": FAILED, "code": "io", "exhausted": False})),
            FAILED_CLASS,
        )

    def test_indeterminate_by_resolution(self) -> None:
        self.assertEqual(
            commit_class(entry(outcome={"outcome": INDETERMINATE, "resolved": True})),
            INDETERMINATE_COMMITTED,
        )
        self.assertEqual(
            commit_class(entry(outcome={"outcome": INDETERMINATE, "resolved": False})),
            INDETERMINATE_ABSENT,
        )
        self.assertEqual(
            commit_class(entry(outcome={"outcome": INDETERMINATE, "resolved": None})), UNRESOLVED
        )


class HistoryTests(unittest.TestCase):
    def test_round_trip_preserves_everything(self) -> None:
        original = history(
            entry(0, 0, touched=(created("order", "a", 1),), values={"total": 3}),
            entry(0, 1, outcome={"outcome": REJECTED, "code": "stale_version"}),
            seed=42,
            workers=2,
        )
        again = History.from_jsonl(original.to_jsonl())
        self.assertEqual(again.header, original.header)
        self.assertEqual(again.entries, original.entries)

    def test_counts_by_class(self) -> None:
        h = history(
            entry(0, 0),
            entry(0, 1, outcome={"outcome": ROLLED_BACK}),
            entry(0, 2, outcome={"outcome": FAILED, "code": "x", "exhausted": True}),
            entry(0, 3, outcome={"outcome": INDETERMINATE, "resolved": None}),
        )
        counts = h.counts()
        self.assertEqual(counts[ACKNOWLEDGED], 1)
        self.assertEqual(counts[REFUSED], 1)
        self.assertEqual(counts[FAILED_CLASS], 1)
        self.assertEqual(counts[UNRESOLVED], 1)

    def test_entries_are_sorted_by_start(self) -> None:
        h = History.from_jsonl(
            history(entry(0, 1, start_us=10), entry(0, 0, start_us=5)).to_jsonl()
        )
        self.assertEqual([e.start_us for e in h.entries], [5, 10])

    def test_bad_format_is_refused(self) -> None:
        text = '{"format":"other","version":1,"seed":0,"workers":1,"profile":"smoke"}\n'
        with self.assertRaises(HistoryError):
            History.from_jsonl(text)

    def test_bad_version_is_refused(self) -> None:
        text = '{"format":"spot-l7-history","version":2,"seed":0,"workers":1,"profile":"smoke"}\n'
        with self.assertRaises(HistoryError):
            History.from_jsonl(text)

    def test_empty_history_is_refused(self) -> None:
        with self.assertRaises(HistoryError):
            History.from_jsonl("\n\n")


class DurableWriterTests(unittest.TestCase):
    def test_appended_entries_are_readable_and_synced(self) -> None:
        directory = support.tmpdir()
        path = os.path.join(directory, "h.jsonl")
        header = Header(seed=7, workers=1, profile="smoke")
        with HistoryWriter(path, header) as writer:
            writer.append(entry(0, 0, touched=(created("party", "p1"),)))
            writer.append(entry(0, 1))
        read = read_history(path)
        self.assertEqual(read.header.seed, 7)
        self.assertEqual(len(read.entries), 2)
        self.assertEqual(read.entries[0].touched, (Touched("party", "p1", "created", 1),))

    def test_writer_truncates_a_previous_run(self) -> None:
        directory = support.tmpdir()
        path = os.path.join(directory, "h.jsonl")
        header = Header(seed=1, workers=1, profile="smoke")
        with HistoryWriter(path, header) as writer:
            writer.append(entry(0, 0))
        with HistoryWriter(path, header) as writer:
            writer.append(entry(0, 0))
        self.assertEqual(len(read_history(path).entries), 1)


if __name__ == "__main__":
    unittest.main()
