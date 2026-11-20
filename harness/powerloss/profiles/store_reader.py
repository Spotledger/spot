#!/usr/bin/env python3
"""The store snapshot reader of the shipped-host profile (WP-PRD-05.T3).

After a restart, reads the committed documents of the posting DocType through the HTTP **list
port** (never SQL), sorted by id, and prints a ``StoreSnapshot`` JSON.

What an HTTP list can and cannot say is stated, not hidden:

- ``documents``: read.
- ``changes``: derived. The posting workload only creates documents, so each present document is
  one ``insert`` at version 1. (A workload that updates documents needs a real feed reader.)
- ``outbox`` and ``sequences``: not readable through the list port, so they are reported
  ``missing`` and the run fails visibly, unless the operator declares them empty with
  ``--declare-empty outbox,sequences`` (an assertion that the workload allocates no sequence
  and emits nothing the checker looks at; the outbox is then covered by the count the extras
  reader takes through the management port).
"""

from __future__ import annotations

import argparse
import os
import sys
from typing import Sequence

HERE = os.path.dirname(os.path.abspath(__file__))
HARNESS = os.path.dirname(HERE)
for _p in (HERE, HARNESS):
    if _p not in sys.path:
        sys.path.insert(0, _p)

import operator_http as oh  # noqa: E402
from storesnapshot import Change, Document, StoreSnapshot  # noqa: E402

UNREADABLE = ("outbox", "sequences")


def read_snapshot(config: oh.Config, transport: oh.Transport, declare_empty: Sequence[str] = ()) -> StoreSnapshot:
    session = oh.Session(config, transport)
    session.login()
    rows = session.list_all()
    documents = tuple(
        Document(config.doctype, str(r["id"]), int(r.get("version", 1)), str(r.get("name", "")))
        for r in rows
        if "id" in r
    )
    changes = tuple(Change(d.entity, d.id, "insert", 1) for d in documents)
    unknown = [p for p in declare_empty if p not in UNREADABLE]
    if unknown:
        raise ValueError(f"--declare-empty names unknown parts: {', '.join(unknown)}")
    missing = tuple(p for p in UNREADABLE if p not in declare_empty)
    return StoreSnapshot(changes=changes, documents=documents, missing=missing)


def main(argv: "Sequence[str] | None" = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--declare-empty", default="", help="comma list of parts the workload never uses: outbox,sequences")
    args = parser.parse_args(list(sys.argv[1:] if argv is None else argv))
    try:
        empty = [p for p in args.declare_empty.split(",") if p]
        snapshot = read_snapshot(oh.Config.from_env(os.environ), oh.urllib_transport(), empty)
    except (ValueError, oh.HttpError) as exc:
        print(f"store_reader: {exc}", file=sys.stderr)
        return 2
    print(snapshot.to_json_text())
    return 0


if __name__ == "__main__":
    sys.exit(main())
