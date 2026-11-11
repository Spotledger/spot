"""A small store that models what a power cut can lose.

A real cut cannot run in CI, so the harness ships a *model* of the durability switches the
campaign asserts on the real server. The model is deliberately narrow and honest:

- a commit is **acknowledged** to the client when the store's ``commit`` returns;
- with ``fsync = on`` **and** ``synchronous_commit = on`` the commit is on durable storage
  before it is acknowledged, so a cut keeps it;
- with ``synchronous_commit = off`` (or ``fsync = off``) the commit is acknowledged while it
  is still only in volatile memory, so a cut **loses** it — the client believed it committed,
  the store did not keep it, and the durability audit must report ``acknowledged_effect_absent``.

This is what makes the negative control real: a campaign with ``fsync = off`` must *fail*.
``full_page_writes`` is asserted on the real server but its failure mode (torn pages) is not
modelled here — a cut drops only the writes the switches say are unsafe.

The store is not the product. It is a model, and the README says so.
"""

from __future__ import annotations

import threading
from dataclasses import dataclass, field
from typing import Iterable

from storesnapshot import (
    DELETE,
    INSERT,
    UPDATE,
    Change,
    ChildRow,
    Document,
    OutboxEvent,
    StoreSnapshot,
)


class StoreError(Exception):
    """A transaction the store refused (nothing happened)."""

    def __init__(self, code: str) -> None:
        super().__init__(code)
        self.code = code


@dataclass
class DocumentState:
    """The current state of one document in the model."""

    entity: str
    id: str
    version: int
    fields: dict[str, object] = field(default_factory=dict)
    links: tuple[tuple[str, str], ...] = ()
    children: tuple[ChildRow, ...] = ()

    def name(self) -> str:
        value = self.fields.get("name")
        return str(value) if isinstance(value, str) else ""


@dataclass
class StagedOp:
    """One write a transaction staged, applied only at commit."""

    kind: str  # insert | update | delete | allocate | enqueue
    entity: str = ""
    id: str = ""
    version: int = 0
    fields: dict[str, object] = field(default_factory=dict)
    links: tuple[tuple[str, str], ...] = ()
    children: tuple[ChildRow, ...] = ()
    series: str = ""
    number: int = 0
    event: OutboxEvent | None = None


class FakeStore:
    """The model store. All public methods take the store's lock."""

    def __init__(self, *, fsync: bool = True, synchronous_commit: bool = True, full_page_writes: bool = True) -> None:
        self.fsync = fsync
        self.synchronous_commit = synchronous_commit
        self.full_page_writes = full_page_writes
        self._lock = threading.RLock()
        # One transaction at a time in the model: this keeps the schedule's outcome
        # deterministic and lets version checks be exact, without pretending to model
        # MVCC. The real concurrency is the real server's job; the model is about durability.
        self._tx_lock = threading.Lock()
        # The visible state (what a reader sees right now, including unsynced commits).
        self._docs: dict[tuple[str, str], DocumentState] = {}
        self._changes: list[Change] = []
        self._outbox: list[OutboxEvent] = []
        self._sequences: dict[str, int] = {}
        # The write-ahead log, and how much of it is on durable storage.
        self._wal: list[list[StagedOp]] = []
        self._synced = 0

    # -- durability model -------------------------------------------------------------------

    def _durable_commit(self) -> bool:
        """Whether a commit reaches durable storage before it is acknowledged."""
        return self.fsync and self.synchronous_commit

    def _apply(self, ops: Iterable[StagedOp]) -> None:
        for op in ops:
            if op.kind == "insert":
                self._docs[(op.entity, op.id)] = DocumentState(
                    entity=op.entity,
                    id=op.id,
                    version=op.version,
                    fields=dict(op.fields),
                    links=op.links,
                    children=op.children,
                )
                self._changes.append(Change(op.entity, op.id, INSERT, op.version))
            elif op.kind == "update":
                state = self._docs[(op.entity, op.id)]
                state.version = op.version
                state.fields = dict(op.fields)
                state.links = op.links
                state.children = op.children
                self._changes.append(Change(op.entity, op.id, UPDATE, op.version))
            elif op.kind == "delete":
                del self._docs[(op.entity, op.id)]
                self._changes.append(Change(op.entity, op.id, DELETE, op.version))
            elif op.kind == "allocate":
                self._sequences[op.series] = max(self._sequences.get(op.series, 1), op.number + 1)
            elif op.kind == "enqueue" and op.event is not None:
                self._outbox.append(op.event)

    def _rebuild(self) -> None:
        """Rebuild the visible state from the durable part of the WAL (recovery)."""
        self._docs = {}
        self._changes = []
        self._outbox = []
        self._sequences = {}
        for ops in self._wal[: self._synced]:
            self._apply(ops)

    def cut(self) -> None:
        """A power cut: everything not on durable storage is gone."""
        with self._lock:
            del self._wal[self._synced :]
            self._rebuild()

    # -- reads ------------------------------------------------------------------------------

    def settings(self) -> dict[str, object]:
        return {
            "fsync": "on" if self.fsync else "off",
            "synchronous_commit": "on" if self.synchronous_commit else "off",
            "full_page_writes": "on" if self.full_page_writes else "off",
        }

    def document(self, entity: str, id: str) -> DocumentState | None:
        with self._lock:
            return self._docs.get((entity, id))

    def snapshot(self) -> StoreSnapshot:
        with self._lock:
            documents = tuple(
                Document(
                    entity=state.entity,
                    id=state.id,
                    version=state.version,
                    name=state.name(),
                    links=state.links,
                    children=state.children,
                )
                for state in self._docs.values()
            )
            return StoreSnapshot(
                changes=tuple(self._changes),
                outbox=tuple(self._outbox),
                documents=documents,
                sequences=dict(self._sequences),
            )

    # -- transactions -----------------------------------------------------------------------

    def begin(self) -> "FakeTx":
        self._tx_lock.acquire()
        return FakeTx(self)

    def _commit(self, ops: list[StagedOp], cut_now: bool) -> bool:
        """Applies a transaction and returns whether it was acknowledged.

        ``cut_now`` models a power cut landing during the commit: the WAL record is written
        but not synced, so it is lost and the client is *not* acknowledged.
        """
        with self._lock:
            self._wal.append(ops)
            if self._durable_commit() and not cut_now:
                self._synced = len(self._wal)
            self._apply(ops)
            return not cut_now


class FakeTx:
    """One transaction on a :class:`FakeStore`, staged until ``commit``."""

    def __init__(self, store: FakeStore) -> None:
        self._store = store
        self._ops: list[StagedOp] = []
        self._closed = False

    def insert(
        self,
        entity: str,
        id: str,
        fields: dict[str, object],
        *,
        links: tuple[tuple[str, str], ...] = (),
        children: tuple[ChildRow, ...] = (),
    ) -> int:
        self._ops.append(
            StagedOp(
                kind="insert",
                entity=entity,
                id=id,
                version=1,
                fields=fields,
                links=links,
                children=children,
            )
        )
        return 1

    def update(
        self,
        entity: str,
        id: str,
        version: int,
        fields: dict[str, object],
        *,
        links: tuple[tuple[str, str], ...] = (),
        children: tuple[ChildRow, ...] = (),
    ) -> int:
        state = self._store.document(entity, id)
        if state is None:
            raise StoreError("not_found")
        if state.version != version:
            raise StoreError("stale_version")
        new_version = version + 1
        self._ops.append(
            StagedOp(
                kind="update",
                entity=entity,
                id=id,
                version=new_version,
                fields=fields,
                links=links,
                children=children,
            )
        )
        return new_version

    def delete(self, entity: str, id: str, version: int) -> None:
        state = self._store.document(entity, id)
        if state is None:
            raise StoreError("not_found")
        if state.version != version:
            raise StoreError("stale_version")
        self._ops.append(StagedOp(kind="delete", entity=entity, id=id, version=version))

    def allocate(self, series: str, count: int) -> int:
        with self._store._lock:
            number = self._store._sequences.get(series, 1)
            self._store._sequences[series] = number + count
        self._ops.append(StagedOp(kind="allocate", series=series, number=number))
        return number

    def enqueue(self, event: OutboxEvent) -> None:
        self._ops.append(StagedOp(kind="enqueue", event=event))

    def commit(self, *, cut_now: bool = False) -> bool:
        if self._closed:
            raise StoreError("closed")
        self._closed = True
        try:
            return self._store._commit(self._ops, cut_now)
        finally:
            self._store._tx_lock.release()

    def rollback(self) -> None:
        if self._closed:
            return
        self._closed = True
        try:
            # A rolled-back allocation is given back so the series stays gapless.
            for op in self._ops:
                if op.kind == "allocate":
                    with self._store._lock:
                        if self._store._sequences.get(op.series, 1) == op.number + 1:
                            self._store._sequences[op.series] = op.number
        finally:
            self._store._tx_lock.release()
