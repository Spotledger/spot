"""Shared test helpers: path bootstrap and small builders.

Every test module imports this first; it puts the harness directory on ``sys.path`` so the
modules (``l7history``, ``checks`` and the rest) import as top-level names, exactly as
``powerloss.py`` imports them.
"""

from __future__ import annotations

import os
import sys
import tempfile

HARNESS_DIR = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
if HARNESS_DIR not in sys.path:
    sys.path.insert(0, HARNESS_DIR)

from l7history import (  # noqa: E402
    COMMITTED,
    CREATED,
    Entry,
    Header,
    History,
    INDETERMINATE,
    READ_COMMITTED,
    REJECTED,
    Touched,
)
from storesnapshot import Change, Document, INSERT, OutboxEvent, StoreSnapshot, UPDATE  # noqa: E402


def entry(
    worker: int = 0,
    seq: int = 0,
    *,
    outcome: dict | None = None,
    kind: dict | None = None,
    touched: tuple[Touched, ...] = (),
    values: dict | None = None,
    start_us: int = 0,
) -> Entry:
    """A history entry with sensible defaults."""
    return Entry(
        worker=worker,
        seq=seq,
        start_us=start_us,
        end_us=start_us + 1,
        kind=kind or {"kind": "create_order", "lines": 1},
        isolation=READ_COMMITTED,
        outcome=outcome or {"outcome": COMMITTED},
        attempts=1,
        touched=touched,
        values=values or {},
    )


def history(*entries: Entry, seed: int = 1, workers: int = 1) -> History:
    return History(header=Header(seed=seed, workers=workers, profile="smoke"), entries=tuple(entries))


def created(entity: str, id: str, version: int = 1) -> Touched:
    return Touched(entity=entity, id=id, role=CREATED, version=version)


def snapshot(**kwargs) -> StoreSnapshot:
    return StoreSnapshot(**kwargs)


def tmpdir() -> str:
    return tempfile.mkdtemp(prefix="powerloss-test-")
