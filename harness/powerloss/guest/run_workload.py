#!/usr/bin/env python3
"""The workload agent that runs **inside** the target (WP-PRD-05.T1).

The orchestrator starts this on the target (over SSH, or through the Hyper-V guest agent)
with the seed, the history path and the cut point. It either delegates to a real workload
client (``--client-cmd``) or runs the harness's own model workload against the model store
(``--store model``), and it always writes the client-side history **durably**, one fsync'd
line per acknowledged operation, so a cut cannot lose the record of a commit the client was
told succeeded.

Real client
-----------
``--client-cmd 'spot-l7 run --backend pg'`` runs the named command with ``--seed``,
``--history`` and ``--cut-after`` appended (argv list, no shell). A real client must write the
same ``spot-l7-history`` v1 format; the L7 CLI writes it at the end of a run, which cannot
survive a mid-run cut, so a power-cut client must stream it — see README.md and the Open
questions in the PR.

Model client
------------
``--store model`` runs the harness's :class:`PostingClient` against the model store. It is a
model, not the product: use it for smoke runs on a target, never as evidence of the real
server's durability.

    python run_workload.py --seed 7 --history C:/pl/history.jsonl --cut-after 500 \\
        --store model --workers 8 --operations 5000
"""

from __future__ import annotations

import argparse
import json
import os
import subprocess
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
HARNESS = os.path.dirname(HERE)
if HARNESS not in sys.path:
    sys.path.insert(0, HARNESS)

from fakestore import FakeStore  # noqa: E402
from workload import PostingClient, WorkloadConfig  # noqa: E402


def monotonic_us() -> int:
    import time

    return time.monotonic_ns() // 1000


def run_model(args: argparse.Namespace) -> int:
    store = FakeStore(
        fsync=args.fsync == "on",
        synchronous_commit=args.synchronous_commit == "on",
        full_page_writes=True,
    )
    config = WorkloadConfig(seed=args.seed, workers=args.workers, operations=args.operations, profile=args.profile)
    client = PostingClient(store, config, args.history, monotonic_us, stop_after=args.cut_after)
    stats = client.run()
    print(
        json.dumps(
            {
                "agent": "run_workload",
                "mode": "model",
                "seed": args.seed,
                "operations": stats.operations,
                "committed": stats.committed,
                "cut": stats.cut,
                "cut_offset_ops": stats.cut_offset_ops,
                "history": args.history,
            },
            sort_keys=True,
        )
    )
    return 0


def run_client(args: argparse.Namespace) -> int:
    argv = list(args.client_cmd.split()) + [
        "--seed",
        str(args.seed),
        "--history",
        args.history,
        "--cut-after",
        str(args.cut_after),
    ]
    completed = subprocess.run(argv, check=False)
    return completed.returncode


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--seed", type=int, required=True)
    parser.add_argument("--history", required=True)
    parser.add_argument("--cut-after", type=int, default=0, help="stop after this many operations (the cut point)")
    parser.add_argument("--workers", type=int, default=8)
    parser.add_argument("--operations", type=int, default=2_000)
    parser.add_argument("--profile", default="smoke")
    parser.add_argument("--store", default="model", choices=("model",))
    parser.add_argument("--fsync", default="on", choices=("on", "off"))
    parser.add_argument("--synchronous-commit", default="on", choices=("on", "off"))
    parser.add_argument("--client-cmd", default="")
    return parser


def main(argv: list[str] | None = None) -> int:
    args = build_parser().parse_args(argv)
    if args.client_cmd:
        return run_client(args)
    return run_model(args)


if __name__ == "__main__":
    sys.exit(main())
