#!/usr/bin/env python3
"""Target adapters for the shipped-host profile (WP-PRD-05.T2).

The harness talks to a target through commands (see ``../README.md``). These are the
commands for the **shipped** host: ``spot serve`` and its bundled PostgreSQL.

    shipped_adapters.py settings                 SHOW fsync / full_page_writes / synchronous_commit, as JSON
    shipped_adapters.py health --addr HOST:PORT  exit 0 when /api/health/ready answers 200
    shipped_adapters.py pids --pg-data DIR [--root DIR | --service NAME]
                                                 the postmaster and ``spot serve`` pids, one per line
    shipped_adapters.py snapshot                 the recovered store as a StoreSnapshot JSON
    shipped_adapters.py extras                   numbering, ledger totals and job runs as JSON

``snapshot`` and ``extras`` run the operator's readers named by ``SPOT_POWERLOSS_SNAPSHOT_READER``
and ``SPOT_POWERLOSS_EXTRAS_READER`` (an argv string, no shell) and **validate** what they print.
With no reader configured they print a document whose every part is ``missing``, so the run
fails visibly instead of passing on nothing. The connection string is read from
``SPOT_POWERLOSS_PG_URL`` and is never printed or passed as an argument.
"""

from __future__ import annotations

import argparse
import os
import shlex
import subprocess
import sys
import urllib.error
import urllib.request
from typing import Mapping, Sequence

HERE = os.path.dirname(os.path.abspath(__file__))
HARNESS = os.path.dirname(HERE)
for path in (HERE, HARNESS):
    if path not in sys.path:
        sys.path.insert(0, path)

import shipped_extras  # noqa: E402
import shipped_settings  # noqa: E402
from storesnapshot import StoreSnapshot  # noqa: E402

SNAPSHOT_READER_ENV = "SPOT_POWERLOSS_SNAPSHOT_READER"
EXTRAS_READER_ENV = "SPOT_POWERLOSS_EXTRAS_READER"
SNAPSHOT_PARTS = ("changes", "outbox", "documents", "sequences")


def _run(argv: Sequence[str], env: Mapping[str, str] | None = None, timeout: float = 60.0) -> str:
    completed = subprocess.run(
        list(argv), capture_output=True, text=True, timeout=timeout, check=False, env=dict(env) if env is not None else None
    )
    if completed.returncode != 0:
        # stderr is trimmed and the URL never reaches it (it is in the environment only).
        raise RuntimeError(f"{argv[0]} exited {completed.returncode}: {completed.stderr.strip()[:300]}")
    return completed.stdout


# -- settings -------------------------------------------------------------------------------


def cmd_settings(args: argparse.Namespace) -> int:
    env = shipped_settings.psql_env(os.environ)
    values = shipped_settings.read_settings(lambda argv: _run(argv, env), args.psql)
    print(shipped_settings.to_json(values))
    return 0


# -- health ---------------------------------------------------------------------------------


def ready_url(addr: str) -> str:
    """``http://HOST:PORT/api/health/ready`` for a plain ``host:port`` address."""
    host, _, port = addr.rpartition(":")
    if not host or not port.isdigit():
        raise ValueError(f"not a host:port address: {addr!r}")
    return f"http://{host}:{port}/api/health/ready"


def cmd_health(args: argparse.Namespace) -> int:
    try:
        with urllib.request.urlopen(ready_url(args.addr), timeout=args.timeout_s) as response:  # noqa: S310 (fixed http scheme)
            return 0 if response.status == 200 else 1
    except (urllib.error.URLError, OSError, ValueError):
        return 1


# -- pids -----------------------------------------------------------------------------------


def parse_postmaster_pid(text: str) -> int | None:
    """The first line of ``postmaster.pid`` is the postmaster's pid."""
    first = text.splitlines()[0].strip() if text.strip() else ""
    return int(first) if first.isdigit() and int(first) > 0 else None


def parse_sc_queryex(text: str) -> int | None:
    """The ``PID`` of ``sc.exe queryex <service>`` output; ``None`` when stopped (0)."""
    for line in text.splitlines():
        key, _, value = line.partition(":")
        if key.strip().upper() == "PID" and value.strip().isdigit():
            pid = int(value.strip())
            return pid if pid > 0 else None
    return None


def match_serve_cmdline(cmdline: Sequence[str], root: str) -> bool:
    """True for ``spot --root ROOT serve`` (the argument order of the shipped launcher)."""
    if "serve" not in cmdline or "--root" not in cmdline:
        return False
    index = cmdline.index("--root")
    return index + 1 < len(cmdline) and os.path.normpath(cmdline[index + 1]) == os.path.normpath(root)


def linux_serve_pids(root: str, proc: str = "/proc") -> list[int]:
    found = []
    try:
        names = os.listdir(proc)
    except OSError:
        return found
    for name in names:
        if not name.isdigit():
            continue
        try:
            with open(os.path.join(proc, name, "cmdline"), "rb") as handle:
                parts = [p.decode("utf-8", "replace") for p in handle.read().split(b"\0") if p]
        except OSError:
            continue
        if parts and os.path.basename(parts[0]) in ("spot", "spot.exe") and match_serve_cmdline(parts, root):
            found.append(int(name))
    return sorted(found)


def cmd_pids(args: argparse.Namespace) -> int:
    pids: list[int] = []
    try:
        with open(os.path.join(args.pg_data, "postmaster.pid"), "r", encoding="utf-8") as handle:
            postmaster = parse_postmaster_pid(handle.read())
    except OSError:
        postmaster = None
    if postmaster is not None:
        pids.append(postmaster)
    if args.service:
        service = _run(["sc.exe", "queryex", args.service])
        pid = parse_sc_queryex(service)
        if pid is not None:
            pids.append(pid)
    elif args.root:
        pids.extend(linux_serve_pids(args.root))
    for pid in pids:
        print(pid)
    return 0 if pids else 1


# -- snapshot and extras --------------------------------------------------------------------


def _reader_output(env_name: str) -> str | None:
    command = os.environ.get(env_name, "").strip()
    if not command:
        return None
    env = shipped_settings.psql_env(os.environ)
    return _run(shlex.split(command), env, timeout=120.0)


def cmd_snapshot(args: argparse.Namespace) -> int:
    text = _reader_output(SNAPSHOT_READER_ENV)
    if text is None:
        print(StoreSnapshot(missing=SNAPSHOT_PARTS).to_json_text())
        return 0
    print(StoreSnapshot.from_json_text(text).to_json_text())
    return 0


def cmd_extras(args: argparse.Namespace) -> int:
    text = _reader_output(EXTRAS_READER_ENV)
    if text is None:
        print(shipped_extras.ExtraSnapshot.unavailable().to_json_text())
        return 0
    print(shipped_extras.ExtraSnapshot.from_json_text(text).to_json_text())
    return 0


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(prog="shipped_adapters.py", description=__doc__.splitlines()[0])
    sub = parser.add_subparsers(dest="command", required=True)
    settings = sub.add_parser("settings")
    settings.add_argument("--psql", default="psql")
    settings.set_defaults(fn=cmd_settings)
    health = sub.add_parser("health")
    health.add_argument("--addr", default="127.0.0.1:8080")
    health.add_argument("--timeout-s", type=float, default=5.0)
    health.set_defaults(fn=cmd_health)
    pids = sub.add_parser("pids")
    pids.add_argument("--pg-data", required=True)
    pids.add_argument("--root", default="")
    pids.add_argument("--service", default="")
    pids.set_defaults(fn=cmd_pids)
    sub.add_parser("snapshot").set_defaults(fn=cmd_snapshot)
    sub.add_parser("extras").set_defaults(fn=cmd_extras)
    return parser


def main(argv: Sequence[str] | None = None) -> int:
    args = build_parser().parse_args(list(sys.argv[1:] if argv is None else argv))
    try:
        return args.fn(args)
    except (RuntimeError, OSError, subprocess.TimeoutExpired, shipped_settings.ShippedSettingsError) as exc:
        print(f"shipped_adapters: {exc}", file=sys.stderr)
        return 2
    except (ValueError, shipped_extras.ExtrasError) as exc:
        print(f"shipped_adapters: invalid reader output: {exc}", file=sys.stderr)
        return 3


if __name__ == "__main__":
    sys.exit(main())
