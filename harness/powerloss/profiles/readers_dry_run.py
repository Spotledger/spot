"""The dry-run exercise of the three operator pieces against the fake API (WP-PRD-05.T3)."""

from __future__ import annotations

import os

import checks
import extras_reader
import fake_api
import l7history
import operator_http as oh
import post_client
import postings_audit
import shipped_extras
import store_reader
from storesnapshot import StoreSnapshot


def config() -> oh.Config:
    return oh.Config("http://fake.invalid", "tester", "pw")


def exercise(workdir: str, seed: int) -> list[str]:
    problems: list[str] = []
    api = fake_api.FakeApi(page=3)
    path = os.path.join(workdir, "posting-history.jsonl")
    clock = iter(range(0, 10**9, 10))
    stats = post_client.run(config(), api.transport, seed, path, 0, 8, lambda: next(clock))
    if stats["committed"] != 8:
        problems.append(f"post client committed {stats['committed']} of 8")
    history = l7history.read_history(path)
    snapshot = store_reader.read_snapshot(config(), api.transport, ["outbox", "sequences"])
    report = checks.check_all(history, snapshot)
    if not report.ok:
        problems.append("clean posting run reported violations: " + ",".join(report.failed_ids()))
    extras = shipped_extras.ExtraSnapshot.from_json(
        extras_reader.read_extras(
            config(), api.transport, lambda: {"head": {"generation": 1, "seq": 8}, "outbox_pending": 0}, ["ledger", "job_runs"]
        )
    )
    if extras.missing or shipped_extras.numbering_violations(extras):
        problems.append("clean extras were not clean")
    # An acknowledged posting missing after restart is reported.
    lost = next(iter(api.docs))
    del api.docs[lost]
    lost_report = checks.check_all(history, store_reader.read_snapshot(config(), api.transport, ["outbox", "sequences"]))
    if lost_report.ok:
        problems.append("a lost acknowledged posting was not reported")
    # An unacknowledged posting that appears is allowed and counted.
    api.docs["extra"] = {"id": "extra", "version": 1, "name": "X"}
    api.docs[lost] = {"id": lost, "version": 1, "name": "back"}
    extra_snapshot = store_reader.read_snapshot(config(), api.transport, ["outbox", "sequences"])
    allowed = checks.check_all(history, extra_snapshot)
    count = postings_audit.unacknowledged_present(history, extra_snapshot)
    if not allowed.ok or count != 1:
        problems.append(f"an unacknowledged posting was not allowed and counted (ok={allowed.ok}, count={count})")
    print(f"operator readers: post client, snapshot reader and extras reader exercised; {count} unacknowledged posting counted")
    return problems
