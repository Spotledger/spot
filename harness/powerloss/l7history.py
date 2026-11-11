"""The client-side history of a posting workload, in the L7 history format.

WP-PRD-05 reuses the *history format* of ``harness/rust/l7-invariants`` (its
``history.rs`` documents it as public and versioned: ``spot-l7-history`` v1). This
module reads and writes exactly that JSON-lines format, so a history produced here can
be read by the L7 tools and a history produced by the L7 driver can be audited here.

The one difference that matters for power loss is **when** the file is written. The L7
CLI writes the whole history after its run ends, which is useless for a cut: the machine
dies before the file is written. This recorder appends each entry and ``fsync``\\ s it
**before the entry is treated as acknowledged**, so the history survives a power cut. A
commit that the store acknowledged but whose history line did not reach the disk is, by
construction, one the client never counted — which is exactly the durability rule
"every acknowledged commit is present".

Times are integer microseconds on one monotonic clock supplied by the caller; the
recorder never reads the wall clock itself (time is an argument).
"""

from __future__ import annotations

import json
import os
import threading
from dataclasses import dataclass, field
from typing import Any, Iterable

HISTORY_FORMAT = "spot-l7-history"
HISTORY_VERSION = 1

# Isolation levels, serializable form (`Level` in history.rs).
READ_COMMITTED = "read_committed"
REPEATABLE_READ = "repeatable_read"
SERIALIZABLE = "serializable"
LEVELS = (READ_COMMITTED, REPEATABLE_READ, SERIALIZABLE)

# How an increment avoids a lost update.
VERSION_CHECK = "version_check"
ROW_LOCK = "row_lock"

# Touched-record roles.
CREATED = "created"
UPDATED = "updated"
DELETED = "deleted"
READ = "read"

# How an operation ended.
COMMITTED = "committed"
ROLLED_BACK = "rolled_back"
REJECTED = "rejected"
FAILED = "failed"
INDETERMINATE = "indeterminate"


class HistoryError(ValueError):
    """The history text is not a supported ``spot-l7-history`` v1 file."""


@dataclass(frozen=True)
class Touched:
    """A record an operation touched."""

    entity: str
    id: str
    role: str
    version: int | None = None

    def to_json(self) -> dict[str, Any]:
        return {"entity": self.entity, "id": self.id, "role": self.role, "version": self.version}

    @staticmethod
    def from_json(raw: dict[str, Any]) -> "Touched":
        return Touched(
            entity=str(raw["entity"]),
            id=str(raw["id"]),
            role=str(raw["role"]),
            version=None if raw.get("version") is None else int(raw["version"]),
        )


@dataclass(frozen=True)
class Entry:
    """One operation of a run, matching ``HistoryEntry`` in history.rs."""

    worker: int
    seq: int
    start_us: int
    end_us: int
    kind: dict[str, Any]
    isolation: str
    outcome: dict[str, Any]
    attempts: int
    touched: tuple[Touched, ...] = ()
    values: dict[str, int] = field(default_factory=dict)

    def to_json(self) -> dict[str, Any]:
        return {
            "worker": self.worker,
            "seq": self.seq,
            "start_us": self.start_us,
            "end_us": self.end_us,
            "kind": self.kind,
            "isolation": self.isolation,
            "outcome": self.outcome,
            "attempts": self.attempts,
            "touched": [t.to_json() for t in self.touched],
            "values": self.values,
        }

    @staticmethod
    def from_json(raw: dict[str, Any]) -> "Entry":
        return Entry(
            worker=int(raw["worker"]),
            seq=int(raw["seq"]),
            start_us=int(raw["start_us"]),
            end_us=int(raw["end_us"]),
            kind=dict(raw["kind"]),
            isolation=str(raw["isolation"]),
            outcome=dict(raw["outcome"]),
            attempts=int(raw["attempts"]),
            touched=tuple(Touched.from_json(t) for t in raw.get("touched", ())),
            values={str(k): int(v) for k, v in raw.get("values", {}).items()},
        )

    @property
    def label(self) -> str:
        """A stable per-operation name, ``w<worker>#<seq>``."""
        return f"w{self.worker}#{self.seq}"

    @property
    def outcome_kind(self) -> str:
        return str(self.outcome.get("outcome"))


# The outcome classes the durability audit partitions operations into. The names match
# ``CommitClass`` in harness/rust/l8-faults/src/audit.rs.
ACKNOWLEDGED = "acknowledged"
REFUSED = "refused"
FAILED_CLASS = "failed"
INDETERMINATE_COMMITTED = "indeterminate_committed"
INDETERMINATE_ABSENT = "indeterminate_absent"
UNRESOLVED = "unresolved"
COMMIT_CLASSES = (
    ACKNOWLEDGED,
    REFUSED,
    FAILED_CLASS,
    INDETERMINATE_COMMITTED,
    INDETERMINATE_ABSENT,
    UNRESOLVED,
)


def commit_class(entry: Entry) -> str:
    """The durability class of one operation, the rule of ``class_of`` in audit.rs."""
    kind = entry.outcome_kind
    if kind == COMMITTED:
        return ACKNOWLEDGED
    if kind in (ROLLED_BACK, REJECTED):
        return REFUSED
    if kind == FAILED:
        return FAILED_CLASS
    if kind == INDETERMINATE:
        resolved = entry.outcome.get("resolved")
        if resolved is True:
            return INDETERMINATE_COMMITTED
        if resolved is False:
            return INDETERMINATE_ABSENT
        return UNRESOLVED
    raise HistoryError(f"unknown outcome {kind!r}")


@dataclass(frozen=True)
class Header:
    """The first line of a history file."""

    seed: int
    workers: int
    profile: str
    format: str = HISTORY_FORMAT
    version: int = HISTORY_VERSION

    def to_json(self) -> dict[str, Any]:
        return {
            "format": self.format,
            "version": self.version,
            "seed": self.seed,
            "workers": self.workers,
            "profile": self.profile,
        }

    @staticmethod
    def from_json(raw: dict[str, Any]) -> "Header":
        if raw.get("format") != HISTORY_FORMAT or raw.get("version") != HISTORY_VERSION:
            raise HistoryError(
                f"unsupported history format {raw.get('format')!r} v{raw.get('version')!r}"
            )
        return Header(
            seed=int(raw["seed"]), workers=int(raw["workers"]), profile=str(raw["profile"])
        )


@dataclass(frozen=True)
class History:
    """A whole history: header plus entries, ordered by ``(start_us, worker, seq)``."""

    header: Header
    entries: tuple[Entry, ...]

    def counts(self) -> dict[str, int]:
        """How many operations fall in each durability class."""
        out = {name: 0 for name in COMMIT_CLASSES}
        for entry in self.entries:
            out[commit_class(entry)] += 1
        return out

    def to_jsonl(self) -> str:
        lines = [json.dumps(self.header.to_json(), sort_keys=True)]
        for entry in self.entries:
            lines.append(json.dumps(entry.to_json(), sort_keys=True))
        return "\n".join(lines) + "\n"

    @staticmethod
    def from_jsonl(text: str) -> "History":
        lines = [line for line in text.splitlines() if line.strip()]
        if not lines:
            raise HistoryError("empty history")
        header = Header.from_json(json.loads(lines[0]))
        entries = [Entry.from_json(json.loads(line)) for line in lines[1:]]
        entries.sort(key=lambda e: (e.start_us, e.worker, e.seq))
        return History(header=header, entries=tuple(entries))


def read_history(path: str) -> History:
    """Reads a history file written by this module or by the L7 driver."""
    with open(path, "r", encoding="utf-8") as handle:
        return History.from_jsonl(handle.read())


class HistoryWriter:
    """Appends history entries and ``fsync``\\ s each one, so a cut cannot lose them.

    The header is written first (and synced). ``append`` is the durability barrier: after it
    returns, the entry is on the disk. The client only treats a commit as acknowledged once
    its entry is durable.
    """

    def __init__(self, path: str, header: Header) -> None:
        self.path = path
        self.header = header
        directory = os.path.dirname(os.path.abspath(path))
        os.makedirs(directory, exist_ok=True)
        self._lock = threading.Lock()
        # O_TRUNC: a fresh run owns the file.
        self._fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o644)
        self._write_line(header.to_json())

    def _write_line(self, obj: dict[str, Any]) -> None:
        line = (json.dumps(obj, sort_keys=True) + "\n").encode("utf-8")
        with self._lock:
            os.write(self._fd, line)
            os.fsync(self._fd)

    def append(self, entry: Entry) -> None:
        """Durably records one operation. Returns only once the line is on disk."""
        self._write_line(entry.to_json())

    def close(self) -> None:
        os.close(self._fd)

    def __enter__(self) -> "HistoryWriter":
        return self

    def __exit__(self, *_exc: object) -> None:
        self.close()


def entries_from_history(history: History) -> Iterable[Entry]:
    """Convenience iterator, kept so callers do not reach into the tuple."""
    return iter(history.entries)
