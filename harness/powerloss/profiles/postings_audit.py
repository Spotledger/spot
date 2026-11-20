"""Counting what the posting audit allows (WP-PRD-05.T3).

A posting present after restart with no history entry is one the client never counted as
acknowledged (the cut fell between the answer and the history append): it is allowed, and counted
here so a run reports it.
"""

from __future__ import annotations

from l7history import History
from storesnapshot import StoreSnapshot


def unacknowledged_present(history: History, snapshot: StoreSnapshot) -> int:
    recorded = {t.id for entry in history.entries for t in entry.touched}
    return sum(1 for d in snapshot.documents if d.id not in recorded)
