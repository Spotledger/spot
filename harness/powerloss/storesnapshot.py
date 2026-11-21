"""A read-only view of the store after recovery, as JSON.

The invariant suite and the durability audit judge the recovered store against the
client-side history. They never talk to a database directly: a *target* produces a
:class:`StoreSnapshot` — the change feed, the outbox, the sequences and the documents —
and this module carries it between the target and the checks.

The shape mirrors what the L7 invariants read from a real backend (the change feed and
outbox feeds, the sequence state, document versions and child/link rows). A target that
cannot produce a part of it says so: the missing part is reported, never guessed.
"""

from __future__ import annotations

import json
from dataclasses import dataclass, field
from typing import Any

# Change operations, matching `ChangeOp` in the storage contract.
INSERT = "insert"
UPDATE = "update"
DELETE = "delete"


class SnapshotError(ValueError):
    """The snapshot JSON is malformed."""


@dataclass(frozen=True)
class Change:
    """One net entry of the change feed: ``(entity, id, op, version)``."""

    entity: str
    id: str
    op: str
    version: int

    def to_json(self) -> dict[str, Any]:
        return {"entity": self.entity, "id": self.id, "op": self.op, "version": self.version}

    @staticmethod
    def from_json(raw: dict[str, Any]) -> "Change":
        return Change(
            entity=str(raw["entity"]),
            id=str(raw["id"]),
            op=str(raw["op"]),
            version=int(raw["version"]),
        )


@dataclass(frozen=True)
class OutboxEvent:
    """One outbox record: an event id and the document it is about."""

    id: str
    topic: str
    subject_entity: str
    subject_id: str

    def to_json(self) -> dict[str, Any]:
        return {
            "id": self.id,
            "topic": self.topic,
            "subject_entity": self.subject_entity,
            "subject_id": self.subject_id,
        }

    @staticmethod
    def from_json(raw: dict[str, Any]) -> "OutboxEvent":
        return OutboxEvent(
            id=str(raw["id"]),
            topic=str(raw["topic"]),
            subject_entity=str(raw["subject_entity"]),
            subject_id=str(raw["subject_id"]),
        )


@dataclass(frozen=True)
class ChildRow:
    """One child row of a document."""

    table: str
    id: str
    qty: int = 0

    def to_json(self) -> dict[str, Any]:
        return {"table": self.table, "id": self.id, "qty": self.qty}

    @staticmethod
    def from_json(raw: dict[str, Any]) -> "ChildRow":
        return ChildRow(table=str(raw["table"]), id=str(raw["id"]), qty=int(raw.get("qty", 0)))


@dataclass(frozen=True)
class Document:
    """One current document: version, unique name, link fields and child rows."""

    entity: str
    id: str
    version: int
    name: str = ""
    links: tuple[tuple[str, str], ...] = ()
    children: tuple[ChildRow, ...] = ()

    def to_json(self) -> dict[str, Any]:
        return {
            "entity": self.entity,
            "id": self.id,
            "version": self.version,
            "name": self.name,
            "links": [list(pair) for pair in self.links],
            "children": [row.to_json() for row in self.children],
        }

    @staticmethod
    def from_json(raw: dict[str, Any]) -> "Document":
        return Document(
            entity=str(raw["entity"]),
            id=str(raw["id"]),
            version=int(raw["version"]),
            name=str(raw.get("name", "")),
            links=tuple((str(k), str(v)) for k, v in raw.get("links", ())),
            children=tuple(ChildRow.from_json(r) for r in raw.get("children", ())),
        )


@dataclass(frozen=True)
class StoreSnapshot:
    """Everything the checks may look at, in one JSON value."""

    changes: tuple[Change, ...] = ()
    outbox: tuple[OutboxEvent, ...] = ()
    documents: tuple[Document, ...] = ()
    sequences: dict[str, int] = field(default_factory=dict)
    # Names of the parts a target could not read; the checks report these, never ignore them.
    missing: tuple[str, ...] = ()

    def to_json(self) -> dict[str, Any]:
        return {
            "changes": [c.to_json() for c in self.changes],
            "outbox": [e.to_json() for e in self.outbox],
            "documents": [d.to_json() for d in self.documents],
            "sequences": self.sequences,
            "missing": list(self.missing),
        }

    def to_json_text(self) -> str:
        return json.dumps(self.to_json(), sort_keys=True)

    @staticmethod
    def from_json(raw: dict[str, Any]) -> "StoreSnapshot":
        try:
            return StoreSnapshot(
                changes=tuple(Change.from_json(c) for c in raw.get("changes", ())),
                outbox=tuple(OutboxEvent.from_json(e) for e in raw.get("outbox", ())),
                documents=tuple(Document.from_json(d) for d in raw.get("documents", ())),
                sequences={str(k): int(v) for k, v in raw.get("sequences", {}).items()},
                missing=tuple(str(m) for m in raw.get("missing", ())),
            )
        except (KeyError, TypeError, ValueError) as exc:
            raise SnapshotError(f"malformed snapshot: {exc}") from exc

    @staticmethod
    def from_json_text(text: str) -> "StoreSnapshot":
        return StoreSnapshot.from_json(json.loads(text))

    def document(self, entity: str, id: str) -> Document | None:
        for doc in self.documents:
            if doc.entity == entity and doc.id == id:
                return doc
        return None
