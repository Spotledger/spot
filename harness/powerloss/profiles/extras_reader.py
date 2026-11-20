#!/usr/bin/env python3
"""The extras reader of the shipped-host profile (WP-PRD-05.T3).

Prints the ``ExtraSnapshot`` JSON of ``shipped_extras`` plus the two facts of the management port:

- ``numbers``: the document names of the posting DocType (series = the DocType), from the HTTP list.
- ``journal_head`` ``[generation, seq]`` and ``outbox_count``: from the management port. The
  management API takes mutual TLS and signed commands, which this script does not implement: it
  runs the operator's management client, ``SPOT_POWERLOSS_MGMT_CMD`` (an argv string, no shell),
  as ``<cmd> journal.status`` and reads its JSON (``head: {generation, seq}`` and
  ``outbox_pending``). A client or field that is absent leaves that part ``missing``.
- ``ledger`` and ``job_runs``: not available from either port; ``missing`` unless declared
  empty with ``--declare-empty ledger,job_runs`` (the posting workload books no ledger and runs
  no job).
"""

from __future__ import annotations

import argparse
import json
import os
import shlex
import subprocess
import sys
from typing import Callable, Mapping, Sequence

HERE = os.path.dirname(os.path.abspath(__file__))
HARNESS = os.path.dirname(HERE)
for _p in (HERE, HARNESS):
    if _p not in sys.path:
        sys.path.insert(0, _p)

import operator_http as oh  # noqa: E402

MGMT_CMD_ENV = "SPOT_POWERLOSS_MGMT_CMD"
DECLARABLE = ("ledger", "job_runs")


def run_mgmt(environ: Mapping[str, str], timeout_s: float = 60.0) -> "dict | None":
    command = environ.get(MGMT_CMD_ENV, "").strip()
    if not command:
        return None
    done = subprocess.run(shlex.split(command) + ["journal.status"], capture_output=True, text=True, timeout=timeout_s, check=False)
    if done.returncode != 0:
        return None
    try:
        value = json.loads(done.stdout)
    except json.JSONDecodeError:
        return None
    return value.get("data", value) if isinstance(value, dict) else None


def read_extras(
    config: oh.Config,
    transport: oh.Transport,
    mgmt: Callable[[], "dict | None"],
    declare_empty: Sequence[str] = (),
) -> dict:
    session = oh.Session(config, transport)
    session.login()
    names = [str(r.get("name", "")) for r in session.list_all() if r.get("name")]
    unknown = [p for p in declare_empty if p not in DECLARABLE]
    if unknown:
        raise ValueError(f"--declare-empty names unknown parts: {', '.join(unknown)}")
    missing = [p for p in DECLARABLE if p not in declare_empty]
    out: dict = {"numbers": {config.doctype: names}, "ledger": {}, "job_runs": {}, "job_states": {}}
    status = mgmt()
    head = status.get("head") if isinstance(status, dict) else None
    outbox = status.get("outbox_pending") if isinstance(status, dict) else None
    if isinstance(head, dict) and isinstance(head.get("generation"), int) and isinstance(head.get("seq"), int):
        out["journal_head"] = [head["generation"], head["seq"]]
    else:
        missing.append("journal_head")
    if isinstance(outbox, int) and not isinstance(outbox, bool):
        out["outbox_count"] = outbox
    else:
        missing.append("outbox_count")
    out["missing"] = missing
    return out


def main(argv: "Sequence[str] | None" = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--declare-empty", default="")
    args = parser.parse_args(list(sys.argv[1:] if argv is None else argv))
    try:
        empty = [p for p in args.declare_empty.split(",") if p]
        doc = read_extras(oh.Config.from_env(os.environ), oh.urllib_transport(), lambda: run_mgmt(os.environ), empty)
    except (ValueError, oh.HttpError, subprocess.TimeoutExpired) as exc:
        print(f"extras_reader: {exc}", file=sys.stderr)
        return 2
    print(json.dumps(doc, sort_keys=True))
    return 0


if __name__ == "__main__":
    sys.exit(main())
