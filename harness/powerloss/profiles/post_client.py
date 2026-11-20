#!/usr/bin/env python3
"""The posting client of the shipped-host profile (WP-PRD-05.T3).

Signs in as the test user, submits postings through ``POST /api/v1/resource/<doctype>`` with an
``Idempotency-Key`` and a client-chosen UUIDv7 id (so the id is known even when the answer is
lost), and appends one fsync'd history line per operation **after** the answer, in the
``spot-l7-history`` v1 format: the HTTP status and the returned name are in the outcome. A cut
between the answer and the append leaves no entry: that posting was never counted as
acknowledged.

    SPOT_POWERLOSS_BASE_URL=... SPOT_POWERLOSS_USER=... SPOT_POWERLOSS_PASSWORD=... \\
      post_client.py --seed 7 --history H.jsonl --cut-after 500 [--fields-json '{...}']

Network errors and 5xx are retried with the same key (safe: the server replays the first answer);
if the retries run out the operation is recorded ``indeterminate`` and unresolved. 4xx is
``rejected``. Time comes from ``--clock`` injection in code; this CLI uses the process monotonic
clock; randomness is the harness's seeded generator.
"""

from __future__ import annotations

import argparse
import json
import os
import sys
import time
from typing import Callable, Sequence

HERE = os.path.dirname(os.path.abspath(__file__))
HARNESS = os.path.dirname(HERE)
for _p in (HERE, HARNESS):
    if _p not in sys.path:
        sys.path.insert(0, _p)

import l7history as h  # noqa: E402
import operator_http as oh  # noqa: E402
from workload import Rng  # noqa: E402

RETRIES = 3


def uuid7(rng: Rng, millis: int) -> str:
    """A UUIDv7 from a millisecond count and the seeded generator (no entropy source)."""
    hi, lo = rng.next_u64(), rng.next_u64()
    value = (millis & ((1 << 48) - 1)) << 80
    value |= 0x7 << 76
    value |= (hi & 0xFFF) << 64
    value |= 0b10 << 62
    value |= lo & ((1 << 62) - 1)
    text = f"{value:032x}"
    return f"{text[:8]}-{text[8:12]}-{text[12:16]}-{text[16:20]}-{text[20:]}"


def run(
    config: oh.Config,
    transport: oh.Transport,
    seed: int,
    history_path: str,
    cut_after: int,
    operations: int,
    clock_us: Callable[[], int],
    fields: "dict | None" = None,
) -> dict:
    session = oh.Session(config, transport)
    session.login()
    rng = Rng(seed)
    stats = {"operations": 0, "committed": 0, "rejected": 0, "indeterminate": 0}
    limit = min(operations, cut_after) if cut_after > 0 else operations
    with h.HistoryWriter(history_path, h.Header(seed=seed, workers=1, profile="shipped-posting")) as writer:
        for seq in range(limit):
            key = f"pl-{seed}-{seq}"
            doc_id = uuid7(rng, seq)
            document = dict(fields or {})
            document["id"] = doc_id
            start = clock_us()
            status, body, attempts = 0, {}, 0
            for attempts in range(1, RETRIES + 1):
                try:
                    status, body = session.create(document, key)
                except oh.HttpError:
                    status, body = 0, {}
                if 200 <= status < 300 or 400 <= status < 500:
                    break
            end = clock_us()
            data = body.get("data", {}) if isinstance(body.get("data", {}), dict) else {}
            kind = {"kind": "post_document", "doctype": config.doctype, "key": key}
            touched: tuple[h.Touched, ...] = ()
            if 200 <= status < 300:
                version = int(data.get("version", 1))
                outcome = {"outcome": h.COMMITTED, "status": status, "name": str(data.get("name", ""))}
                touched = (h.Touched(config.doctype, doc_id, h.CREATED, version),)
                stats["committed"] += 1
            elif 400 <= status < 500:
                outcome = {"outcome": h.REJECTED, "status": status}
                stats["rejected"] += 1
            else:
                outcome = {"outcome": h.INDETERMINATE, "status": status, "resolved": None}
                touched = (h.Touched(config.doctype, doc_id, h.CREATED, 1),)
                stats["indeterminate"] += 1
            # The append is the durability barrier: only now is the posting counted.
            writer.append(
                h.Entry(0, seq, start, end, kind, h.READ_COMMITTED, outcome, attempts, touched, {})
            )
            stats["operations"] += 1
    return stats


def main(argv: "Sequence[str] | None" = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--seed", type=int, required=True)
    parser.add_argument("--history", required=True)
    parser.add_argument("--cut-after", type=int, default=0)
    parser.add_argument("--operations", type=int, default=100_000)
    parser.add_argument("--fields-json", default="{}", help="extra fields of every posting (the DocType's required fields)")
    args = parser.parse_args(list(sys.argv[1:] if argv is None else argv))
    try:
        config = oh.Config.from_env(os.environ)
        fields = json.loads(args.fields_json)
        stats = run(
            config, oh.urllib_transport(), args.seed, args.history, args.cut_after, args.operations,
            lambda: time.monotonic_ns() // 1000, fields,
        )
    except (ValueError, oh.HttpError) as exc:
        print(f"post_client: {exc}", file=sys.stderr)
        return 2
    print(json.dumps(stats, sort_keys=True))
    return 0


if __name__ == "__main__":
    sys.exit(main())
