"""The posting workload and its client-side history.

This is the posting workload of ``harness/rust/l7-invariants`` expressed over the model
store, with the same operation mix, the same deterministic schedule (a seeded xorshift
RNG, exactly the one in ``schedule.rs``) and the same history format. It differs in the one
way power loss demands: it writes each operation to the history **as it is acknowledged**,
fsync'd, instead of at the end of the run — a client that is killed by a cut still leaves a
durable record of everything it was told committed.

The client never reads the wall clock: ``clock`` is a zero-argument callable returning
microseconds, supplied by the caller. A cut is injected either by ``stop_after`` (a seeded
number of acknowledged operations) or by an external ``cut_event``; both model the machine
dying, so the run does not finish and the history simply ends where it ended.
"""

from __future__ import annotations

import threading
from dataclasses import dataclass, field
from typing import Callable, Sequence

from fakestore import FakeStore, FakeTx, StoreError
from l7history import (
    COMMITTED,
    CREATED,
    DELETED,
    INDETERMINATE,
    READ,
    READ_COMMITTED,
    REJECTED,
    ROLLED_BACK,
    SERIALIZABLE,
    REPEATABLE_READ,
    ROW_LOCK,
    UPDATED,
    VERSION_CHECK,
    Entry,
    Header,
    HistoryWriter,
    Touched,
)
from storesnapshot import ChildRow, OutboxEvent

PARTY = "party"
ORDER = "order"
COUNTER = "counter"
TICKET = "ticket"
TOPIC_COMMIT = "commit"
TOPIC_NOTE = "note"
SERIES_COUNT = 3


class Rng:
    """The seeded xorshift generator of ``schedule.rs`` (splitmix64 seeding)."""

    _MASK = (1 << 64) - 1

    def __init__(self, seed: int) -> None:
        z = (seed + 0x9E3779B97F4A7C15) & self._MASK
        z = ((z ^ (z >> 30)) * 0xBF58476D1CE4E5B9) & self._MASK
        z = ((z ^ (z >> 27)) * 0x94D049BB133111EB) & self._MASK
        z ^= z >> 31
        self._state = z if z != 0 else 0x2545F4914F6CDD1D

    def next_u64(self) -> int:
        s = self._state
        s ^= s >> 12
        s ^= (s << 25) & self._MASK
        s ^= s >> 27
        self._state = s
        return (s * 0x2545F4914F6CDD1D) & self._MASK

    def below(self, n: int) -> int:
        return 0 if n <= 0 else self.next_u64() % n

    def chance(self, num: int, den: int) -> bool:
        return self.below(den) < num


@dataclass(frozen=True)
class Weights:
    """Relative weights of the operation mix (the default of ``schedule.rs``)."""

    create_party: int = 4
    create_order: int = 14
    create_linked: int = 12
    update_order: int = 16
    delete_order: int = 5
    delete_party: int = 2
    increment: int = 20
    allocate: int = 14
    emit: int = 5
    read: int = 8

    def total(self) -> int:
        return sum(self.as_tuple())

    def as_tuple(self) -> tuple[int, ...]:
        return (
            self.create_party,
            self.create_order,
            self.create_linked,
            self.update_order,
            self.delete_order,
            self.delete_party,
            self.increment,
            self.allocate,
            self.emit,
            self.read,
        )


@dataclass(frozen=True)
class PlannedOp:
    """One planned operation: what to do, at which isolation, and a pick."""

    kind: dict[str, object]
    isolation: str
    pick: int


def _level(rng: Rng) -> str:
    roll = rng.below(10)
    if roll <= 4:
        return READ_COMMITTED
    if roll <= 8:
        return REPEATABLE_READ
    return SERIALIZABLE


def plan_one(rng: Rng, weights: Weights) -> PlannedOp:
    """Plans one operation, mirroring ``plan_one`` in ``schedule.rs``."""
    roll = rng.below(max(weights.total(), 1))

    def take(weight: int) -> bool:
        nonlocal roll
        if roll < weight:
            return True
        roll -= weight
        return False

    pick = rng.next_u64()
    if take(weights.create_party):
        kind: dict[str, object] = {"kind": "create_party"}
        isolation = READ_COMMITTED
    elif take(weights.create_order):
        kind = {"kind": "create_order", "lines": rng.below(4)}
        isolation = _level(rng)
    elif take(weights.create_linked):
        kind = {"kind": "create_linked_order", "lines": rng.below(4), "watch": rng.chance(1, 3)}
        isolation = _level(rng)
    elif take(weights.update_order):
        kind = {"kind": "update_order"}
        isolation = _level(rng)
    elif take(weights.delete_order):
        kind = {"kind": "delete_order"}
        isolation = _level(rng)
    elif take(weights.delete_party):
        kind = {"kind": "delete_party"}
        isolation = READ_COMMITTED
    elif take(weights.increment):
        kind = {"kind": "increment_counter", "mode": VERSION_CHECK if rng.chance(1, 2) else ROW_LOCK}
        isolation = _level(rng)
    elif take(weights.allocate):
        kind = {"kind": "allocate_sequence", "series": rng.below(SERIES_COUNT), "rollback": rng.chance(1, 4)}
        isolation = _level(rng)
    elif take(weights.emit):
        kind = {"kind": "emit", "events": rng.below(3)}
        isolation = READ_COMMITTED
    else:
        kind = {"kind": "read"}
        isolation = REPEATABLE_READ
    return PlannedOp(kind=kind, isolation=isolation, pick=pick)


def generate_schedule(seed: int, workers: int, operations: int, weights: Weights) -> list[list[PlannedOp]]:
    """Plans ``operations`` operations over ``workers`` workers (the ``schedule.rs`` split)."""
    workers = max(workers, 1)
    plan: list[list[PlannedOp]] = []
    for worker in range(workers):
        rng = Rng((seed ^ ((worker + 1) * 0xD6E8FEB86659FD93)) & ((1 << 64) - 1))
        share = operations // workers + (1 if worker < operations % workers else 0)
        plan.append([plan_one(rng, weights) for _ in range(share)])
    return plan


@dataclass
class WorkloadConfig:
    """What to run."""

    seed: int
    workers: int = 8
    operations: int = 2_000
    profile: str = "smoke"
    weights: Weights = field(default_factory=Weights)


@dataclass
class RunStats:
    """What a run left behind."""

    operations: int
    committed: int
    refused: int
    failed: int
    cut: bool
    cut_offset_ops: int | None


class IdGen:
    """Deterministic ids, so a seeded run is reproducible."""

    def __init__(self, seed: int) -> None:
        self._lock = threading.Lock()
        self._counter = 0
        self._prefix = f"{seed & 0xFFFFFFFFFFFFFFFF:016x}"

    def next(self) -> str:
        with self._lock:
            self._counter += 1
            return f"{self._prefix}-{self._counter:08x}"


@dataclass
class _Pools:
    parties: list[str] = field(default_factory=list)
    orders: list[str] = field(default_factory=list)
    counters: list[str] = field(default_factory=list)


def _pick(items: Sequence[str], pick: int) -> str | None:
    if not items:
        return None
    return items[pick % len(items)]


class PostingClient:
    """Runs the posting workload against a :class:`FakeStore`, recording a durable history."""

    def __init__(
        self,
        store: FakeStore,
        config: WorkloadConfig,
        history_path: str,
        clock: Callable[[], int],
        *,
        stop_after: int | None = None,
        cut_event: threading.Event | None = None,
    ) -> None:
        self.store = store
        self.config = config
        self.history_path = history_path
        self.clock = clock
        self.stop_after = stop_after
        self.cut_event = cut_event or threading.Event()
        self.ids = IdGen(config.seed)
        self._pools = _Pools()
        self._pools_lock = threading.Lock()
        self._counts = {"committed": 0, "refused": 0, "failed": 0}
        self._counts_lock = threading.Lock()
        self._ops_done = 0
        self._cut = False

    def run(self) -> RunStats:
        schedule = generate_schedule(
            self.config.seed, self.config.workers, self.config.operations, self.config.weights
        )
        header = Header(seed=self.config.seed, workers=self.config.workers, profile=self.config.profile)
        writer = HistoryWriter(self.history_path, header)
        self._setup(writer)
        threads = []
        for worker, ops in enumerate(schedule):
            thread = threading.Thread(target=self._run_worker, args=(writer, worker, ops), daemon=True)
            threads.append(thread)
        for thread in threads:
            thread.start()
        for thread in threads:
            thread.join()
        writer.close()
        counts = dict(self._counts)
        return RunStats(
            operations=counts["committed"] + counts["refused"] + counts["failed"],
            committed=counts["committed"],
            refused=counts["refused"],
            failed=counts["failed"],
            cut=self._cut,
            cut_offset_ops=self._ops_done if self._cut else None,
        )

    # -- setup ------------------------------------------------------------------------------

    def _setup(self, writer: HistoryWriter) -> None:
        """Seeds parties and counters, the way the L7 driver's setup does."""
        tx = self.store.begin()
        parties = [self.ids.next() for _ in range(4)]
        counters = [self.ids.next() for _ in range(4)]
        marker = self.ids.next()
        for index, party in enumerate(parties):
            self._marker(tx, PARTY, party, "setup")
            tx.insert(PARTY, party, {"name": f"party-setup-{index}-{marker}", "marker": f"setup-{index}-{marker}"})
        for index, counter in enumerate(counters):
            self._marker(tx, COUNTER, counter, "setup")
            tx.insert(COUNTER, counter, {"label": f"counter-{index}", "value": 0})
        tx.commit()
        with self._pools_lock:
            self._pools.parties = parties
            self._pools.counters = counters

    def _marker(self, tx: FakeTx, entity: str, id: str, what: str) -> str:
        event_id = self.ids.next()
        tx.enqueue(
            OutboxEvent(id=event_id, topic=TOPIC_COMMIT, subject_entity=entity, subject_id=id)
        )
        return event_id

    # -- workers ----------------------------------------------------------------------------

    def _run_worker(self, writer: HistoryWriter, worker: int, ops: list[PlannedOp]) -> None:
        rng = Rng(
            (self.config.seed ^ ((worker + 1) * 0xA0761D6478BD642F) ^ 0x5851F42D4C957F2D)
            & ((1 << 64) - 1)
        )
        for seq, op in enumerate(ops):
            if self._should_cut():
                self._cut = True
                return
            entry = self._run_op(worker, seq, op, rng)
            # The durability barrier: the entry is on disk before it counts.
            writer.append(entry)
            with self._counts_lock:
                self._ops_done += 1
                if entry.outcome["outcome"] == COMMITTED:
                    self._counts["committed"] += 1
                elif entry.outcome["outcome"] in (ROLLED_BACK, REJECTED):
                    self._counts["refused"] += 1
                else:
                    self._counts["failed"] += 1

    def _should_cut(self) -> bool:
        if self.cut_event.is_set():
            return True
        return self.stop_after is not None and self._ops_done >= self.stop_after

    def _run_op(self, worker: int, seq: int, op: PlannedOp, rng: Rng) -> Entry:
        start_us = self.clock()
        touched: list[Touched] = []
        values: dict[str, int] = {}
        tx = self.store.begin()
        try:
            outcome = self._execute(tx, op, touched, values)
        except StoreError as exc:
            try:
                tx.rollback()
            except StoreError:
                pass
            outcome = {"outcome": REJECTED, "code": exc.code}
        if outcome["outcome"] == "commit":
            acked = tx.commit()
            if acked:
                outcome = {"outcome": COMMITTED}
            else:
                outcome = {"outcome": INDETERMINATE, "resolved": None}
        elif outcome["outcome"] == "rollback":
            tx.rollback()
            outcome = {"outcome": ROLLED_BACK}
        return Entry(
            worker=worker,
            seq=seq,
            start_us=start_us,
            end_us=self.clock(),
            kind=op.kind,
            isolation=op.isolation,
            outcome=outcome,
            attempts=1,
            touched=tuple(touched),
            values=values,
        )

    def _execute(
        self, tx: FakeTx, op: PlannedOp, touched: list[Touched], values: dict[str, int]
    ) -> dict[str, object]:
        """Runs one operation, appending to ``touched``/``values``; returns a sentinel."""
        kind = op.kind["kind"]
        if kind == "create_party":
            id = self.ids.next()
            self._marker(tx, PARTY, id, "create_party")
            marker = self.ids.next()
            tx.insert(PARTY, id, {"name": f"party-{marker}", "marker": marker})
            touched.append(Touched(PARTY, id, CREATED, 1))
            return {"outcome": "commit"}
        if kind == "create_order":
            id = self.ids.next()
            marker = self._marker(tx, ORDER, id, "create_order")
            self._insert_order(tx, touched, id, marker, int(op.kind["lines"]), None, None)
            return {"outcome": "commit"}
        if kind == "create_linked_order":
            with self._pools_lock:
                party = _pick(self._pools.parties, op.pick)
                watched = _pick(self._pools.parties, op.pick >> 20)
            if party is None or self.store.document(PARTY, party) is None:
                return {"outcome": "rollback"}
            if watched is not None and self.store.document(PARTY, watched) is None:
                watched = None
            id = self.ids.next()
            marker = self._marker(tx, ORDER, id, "create_linked_order")
            watch = watched if op.kind.get("watch") else None
            self._insert_order(tx, touched, id, marker, int(op.kind["lines"]), party, watch)
            return {"outcome": "commit"}
        if kind == "update_order":
            with self._pools_lock:
                id = _pick(self._pools.orders, op.pick)
            if id is None:
                return {"outcome": "rollback"}
            self._marker(tx, ORDER, id, "update_order")
            doc = self.store.document(ORDER, id)
            if doc is None:
                return {"outcome": "rollback"}
            total = int(doc.fields.get("total", 0)) + 1
            rows = list(doc.children) + [ChildRow(table="lines", id=self.ids.next(), qty=1)]
            version = tx.update(ORDER, id, doc.version, {"total": total, "tag": f"v{doc.version + 1}"}, links=doc.links, children=tuple(rows))
            touched.append(Touched(ORDER, id, UPDATED, version))
            values["total"] = total
            return {"outcome": "commit"}
        if kind == "delete_order":
            with self._pools_lock:
                id = _pick(self._pools.orders, op.pick)
            if id is None:
                return {"outcome": "rollback"}
            return self._delete(tx, touched, ORDER, id, "delete_order")
        if kind == "delete_party":
            with self._pools_lock:
                id = _pick(self._pools.parties, op.pick)
                orders = list(self._pools.orders)
            if id is None:
                return {"outcome": "rollback"}
            # A party a database-checked link points at is refused, like the L7 driver.
            for order_id in orders:
                doc = self.store.document(ORDER, order_id)
                if doc is not None and any(target == id for _, target in doc.links):
                    return {"outcome": "rollback"}
            return self._delete(tx, touched, PARTY, id, "delete_party")
        if kind == "increment_counter":
            with self._pools_lock:
                id = _pick(self._pools.counters, op.pick)
            if id is None:
                return {"outcome": "rollback"}
            self._marker(tx, COUNTER, id, "increment")
            doc = self.store.document(COUNTER, id)
            if doc is None:
                return {"outcome": "rollback"}
            value = int(doc.fields.get("value", 0)) + 1
            version = tx.update(COUNTER, id, doc.version, {"label": doc.fields.get("label", ""), "value": value}, links=doc.links, children=doc.children)
            touched.append(Touched(COUNTER, id, UPDATED, version))
            values["value"] = value
            return {"outcome": "commit"}
        if kind == "allocate_sequence":
            series = f"l7-series-{int(op.kind['series'])}"
            ticket = self.ids.next()
            marker = self._marker(tx, TICKET, ticket, "allocate")
            number = tx.allocate(series, 1)
            tx.insert(TICKET, ticket, {"series": series, "number": number, "marker": marker})
            touched.append(Touched(TICKET, ticket, CREATED, 1))
            values["number"] = number
            order = self.ids.next()
            self._insert_order(tx, touched, order, f"{marker}-order", 1, None, None)
            if op.kind.get("rollback"):
                return {"outcome": "rollback"}
            return {"outcome": "commit"}
        if kind == "emit":
            with self._pools_lock:
                id = _pick(self._pools.orders, op.pick)
            entity = ORDER if id is not None else PARTY
            if id is None:
                id = self.ids.next()
            self._marker(tx, entity, id, "emit")
            for _ in range(int(op.kind["events"])):
                event_id = self.ids.next()
                tx.enqueue(OutboxEvent(id=event_id, topic=TOPIC_NOTE, subject_entity=entity, subject_id=id))
            values["events"] = int(op.kind["events"])
            return {"outcome": "commit"}
        if kind == "read":
            with self._pools_lock:
                id = _pick(self._pools.orders, op.pick)
            if id is None:
                return {"outcome": "rollback"}
            doc = self.store.document(ORDER, id)
            if doc is None:
                return {"outcome": "rollback"}
            total = int(doc.fields.get("total", -1))
            lines_sum = sum(row.qty for row in doc.children)
            touched.append(Touched(ORDER, id, READ, doc.version))
            values["total"] = total
            values["lines_sum"] = lines_sum
            return {"outcome": "commit"}
        return {"outcome": "rollback"}

    def _insert_order(
        self,
        tx: FakeTx,
        touched: list[Touched],
        id: str,
        marker: str,
        lines: int,
        party: str | None,
        watch: str | None,
    ) -> None:
        rows = tuple(ChildRow(table="lines", id=self.ids.next(), qty=i + 1) for i in range(lines))
        total = sum(range(1, lines + 1))
        links = (("party_link", party or ""), ("watch_link", watch or ""))
        tx.insert(ORDER, id, {"marker": marker, "total": total, "tag": ""}, links=links, children=rows)
        touched.append(Touched(ORDER, id, CREATED, 1))
        with self._pools_lock:
            self._pools.orders.append(id)

    def _delete(self, tx: FakeTx, touched: list[Touched], entity: str, id: str, what: str) -> dict[str, object]:
        self._marker(tx, entity, id, what)
        doc = self.store.document(entity, id)
        if doc is None:
            return {"outcome": "rollback"}
        tx.delete(entity, id, doc.version)
        touched.append(Touched(entity, id, DELETED, doc.version))
        with self._pools_lock:
            if entity == PARTY and id in self._pools.parties:
                self._pools.parties.remove(id)
            elif entity == ORDER and id in self._pools.orders:
                self._pools.orders.remove(id)
        return {"outcome": "commit"}
