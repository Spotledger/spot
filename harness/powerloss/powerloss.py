#!/usr/bin/env python3
"""spot-powerloss: the power-loss and crash-consistency harness (WP-PRD-05.T1).

A real power cut is approximated by hard-killing a VM (or the process tree plus filesystem
barrier loss where a VM is unavailable). This orchestrator runs the posting workload against
the service in a disposable target, cuts at a seeded random point, boots, waits for recovery,
then runs the invariant suite and the durability check against the client-side history.

    python powerloss.py --selftest
    python powerloss.py campaign --cycles 1000 --seed 7 --driver vm-reset \\
        --target vm --vm windows-vm --hypervisor hyperv --out results.jsonl
    python powerloss.py campaign --cycles 20 --target local \\
        --workload-cmd '...' --snapshot-cmd '...' --settings-cmd '...'

Subcommands:
    selftest          run 20 ProcessKill cycles on a real target if configured, else the model
    campaign          run N cycles, resumable, JSON-lines results and a summary table
    cycle             run exactly one cycle and print its report
    negative-control  run with fsync=off and require the audit to find a violation
    drivers           list the cut drivers

The harness only cuts a target it provisioned itself; a target that is not marked disposable
is refused. See README.md for the 1,000-cut campaign on the Windows runner.
"""

from __future__ import annotations

import argparse
import json
import os
import shutil
import sys
import tempfile
from typing import Sequence

HERE = os.path.dirname(os.path.abspath(__file__))
if HERE not in sys.path:
    sys.path.insert(0, HERE)

import campaign as campaign_mod  # noqa: E402
import cutdrivers  # noqa: E402
from targets import (  # noqa: E402
    CommandTarget,
    FakeTarget,
    TargetError,
    run_cycle,
    ssh_runner,
    subprocess_runner,
)
from workload import WorkloadConfig  # noqa: E402

SELFTEST_CYCLES = 20
SELFTEST_OPERATIONS = 60
SELFTEST_WORKERS = 2


def monotonic_us() -> int:
    """Microseconds from a monotonic clock. The only place the harness reads a clock."""
    import time

    return time.monotonic_ns() // 1000


# -- target construction --------------------------------------------------------------------


def fake_target(args: argparse.Namespace, *, history_path: str, operations: int | None = None, workers: int | None = None) -> FakeTarget:
    """The model target, with the durability switches the flags ask for."""
    durability = {
        "fsync": args.fsync == "on",
        "synchronous_commit": args.synchronous_commit == "on",
        "full_page_writes": args.full_page_writes == "on",
    }
    return FakeTarget(
        durability=durability,
        config=WorkloadConfig(
            seed=0,
            workers=workers if workers is not None else args.workers,
            operations=operations if operations is not None else args.operations,
            profile=args.profile,
        ),
        history_path=history_path,
        clock=monotonic_us,
    )


def command_target(args: argparse.Namespace, *, history_path: str) -> CommandTarget:
    """A real target built from the command flags (Linux, over SSH into a VM)."""
    if not args.workload_cmd:
        raise TargetError("--workload-cmd is required for --target local/vm")
    if not args.confirm_disposable:
        raise TargetError(
            "refusing to cut a real target without --confirm-disposable: the harness cannot "
            "tell a throwaway VM from a machine you care about, so you must say it is one"
        )
    guest = (
        ssh_runner(args.guest_host, args.guest_user)
        if args.guest_host
        else subprocess_runner
    )
    return CommandTarget(
        name=args.target,
        runner=subprocess_runner,
        guest_runner=guest,
        clock=monotonic_us,
        history_path=history_path,
        workload_argv=tuple(args.workload_cmd.split()),
        snapshot_argv=tuple(args.snapshot_cmd.split()) if args.snapshot_cmd else (),
        settings_argv=tuple(args.settings_cmd.split()) if args.settings_cmd else (),
        health_argv=tuple(args.health_cmd.split()) if args.health_cmd else (),
        pids_argv=tuple(args.pids_cmd.split()) if args.pids_cmd else (),
        boot_argv=tuple(args.boot_cmd.split()) if args.boot_cmd else (),
        disposable=args.confirm_disposable,
        vm_name=args.vm or "",
        device=args.device or "",
    )


def build_target(args: argparse.Namespace, history_path: str) -> object:
    if args.target == "fake":
        return fake_target(args, history_path=history_path)
    return command_target(args, history_path=history_path)


# -- subcommands ----------------------------------------------------------------------------


def cmd_drivers(args: argparse.Namespace) -> int:
    for name in sorted(cutdrivers.DRIVERS):
        driver = cutdrivers.DRIVERS[name]()
        print(f"{driver.name:<22} [{driver.platform:<7}] {driver.description}")
    return 0


def cmd_cycle(args: argparse.Namespace) -> int:
    workdir = tempfile.mkdtemp(prefix="powerloss-cycle-")
    try:
        history_path = os.path.join(workdir, "history.jsonl")
        target = build_target(args, history_path)
        report, recovery, settings = run_cycle(
            target,
            args.driver,
            seed=args.seed,
            cut_after_ops=args.cut_after,
            recovery_timeout_us=args.recovery_timeout_s * 1_000_000,
            enforce_settings=not args.allow_unsafe_settings,
            os_name=os.name if os.name == "nt" else "linux",
        )
        print(report.to_text())
        print(
            f"recovery: {recovery.recovery_us} us, wal replay: {recovery.wal_replay_us} us, "
            f"settings: {json.dumps(settings, sort_keys=True)}"
        )
        return 0 if report.ok else 1
    finally:
        shutil.rmtree(workdir, ignore_errors=True)


def _campaign_config(args: argparse.Namespace, out_path: str) -> campaign_mod.CampaignConfig:
    return campaign_mod.CampaignConfig(
        campaign_seed=args.seed,
        cycles=args.cycles,
        operations=args.operations,
        workers=args.workers,
        profile=args.profile,
        driver=args.driver,
        cut_min_ops=args.cut_min_ops,
        recovery_timeout_us=args.recovery_timeout_s * 1_000_000,
        enforce_settings=not args.allow_unsafe_settings,
        out_path=out_path,
    )


def cmd_campaign(args: argparse.Namespace) -> int:
    workdir = tempfile.mkdtemp(prefix="powerloss-campaign-")
    out_path = args.out or os.path.join(workdir, "results.jsonl")
    try:
        history_path = os.path.join(workdir, "history.jsonl")
        target = build_target(args, history_path)
        config = _campaign_config(args, out_path)
        summary = campaign_mod.run_campaign(target, config, os_name="windows" if os.name == "nt" else "linux")
        print(summary.to_text())
        print("classification totals:", json.dumps(summary.classification_totals(), sort_keys=True))
        return 0 if summary.violations == 0 else 1
    finally:
        shutil.rmtree(workdir, ignore_errors=True)


def cmd_negative_control(args: argparse.Namespace) -> int:
    """Runs with fsync=off and requires the audit to find a violation."""
    workdir = tempfile.mkdtemp(prefix="powerloss-negative-")
    out_path = args.out or os.path.join(workdir, "results.jsonl")
    try:
        history_path = os.path.join(workdir, "history.jsonl")
        args.fsync = "off"
        args.synchronous_commit = "off"
        args.allow_unsafe_settings = True
        target = build_target(args, history_path)
        config = _campaign_config(args, out_path)
        summary = campaign_mod.run_campaign(target, config, os_name="windows" if os.name == "nt" else "linux")
        print(summary.to_text())
        if summary.violations == 0:
            print(
                "NEGATIVE CONTROL FAILED: with fsync=off no violation was found. The audit is "
                "blind and a green campaign would prove nothing.",
                file=sys.stderr,
            )
            return 1
        print(f"NEGATIVE CONTROL OK: detected {summary.violations} violation(s) with fsync=off.")
        return 0
    finally:
        shutil.rmtree(workdir, ignore_errors=True)


def _selftest_target(workdir: str, args: argparse.Namespace) -> tuple[object, str]:
    """A real target if one is configured, else the model. Returns (target, description)."""
    history_path = os.path.join(workdir, "history.jsonl")
    workload = os.environ.get("SPOT_POWERLOSS_WORKLOAD_CMD", "")
    if workload:
        args.workload_cmd = workload
        args.snapshot_cmd = os.environ.get("SPOT_POWERLOSS_SNAPSHOT_CMD", "")
        args.settings_cmd = os.environ.get("SPOT_POWERLOSS_SETTINGS_CMD", "")
        args.health_cmd = os.environ.get("SPOT_POWERLOSS_HEALTH_CMD", "")
        args.pids_cmd = os.environ.get("SPOT_POWERLOSS_PIDS_CMD", "")
        args.guest_host = os.environ.get("SPOT_POWERLOSS_GUEST_HOST", "")
        args.guest_user = os.environ.get("SPOT_POWERLOSS_GUEST_USER", "")
        try:
            return command_target(args, history_path=history_path), "real target (SPOT_POWERLOSS_WORKLOAD_CMD)"
        except TargetError as exc:
            print(f"selftest: real target unavailable ({exc}); using the model target", file=sys.stderr)
    return fake_target(args, history_path=history_path, operations=SELFTEST_OPERATIONS, workers=SELFTEST_WORKERS), "model target"


def cmd_selftest(args: argparse.Namespace) -> int:
    workdir = tempfile.mkdtemp(prefix="powerloss-selftest-")
    try:
        args.cycles = SELFTEST_CYCLES
        args.operations = SELFTEST_OPERATIONS
        args.workers = SELFTEST_WORKERS
        args.cut_min_ops = 1
        args.allow_unsafe_settings = False
        target, description = _selftest_target(workdir, args)
        print(f"selftest: {SELFTEST_CYCLES} ProcessKill cycles against the {description}")

        out_path = os.path.join(workdir, "positive.jsonl")
        config = campaign_mod.CampaignConfig(
            campaign_seed=args.seed,
            cycles=SELFTEST_CYCLES,
            operations=SELFTEST_OPERATIONS,
            workers=SELFTEST_WORKERS,
            profile="smoke",
            driver="process-kill",
            cut_min_ops=1,
            recovery_timeout_us=30_000_000,
            enforce_settings=True,
            out_path=out_path,
        )
        positive = campaign_mod.run_campaign(target, config, os_name="windows" if os.name == "nt" else "linux")
        print(positive.to_text())
        positive_ok = positive.violations == 0 and positive.cycles_run == SELFTEST_CYCLES

        # The negative control: fsync off, the audit must fail.
        args.fsync = "off"
        args.synchronous_commit = "off"
        negative_target = fake_target(args, history_path=os.path.join(workdir, "neg-history.jsonl"))
        negative_config = campaign_mod.CampaignConfig(
            campaign_seed=args.seed,
            cycles=3,
            operations=SELFTEST_OPERATIONS,
            workers=SELFTEST_WORKERS,
            profile="smoke",
            driver="process-kill",
            cut_min_ops=1,
            recovery_timeout_us=30_000_000,
            enforce_settings=False,
            out_path=os.path.join(workdir, "negative.jsonl"),
        )
        negative = campaign_mod.run_campaign(
            negative_target, negative_config, os_name="windows" if os.name == "nt" else "linux"
        )
        negative_ok = negative.violations > 0
        print(
            f"negative control (fsync=off): {negative.violations} violation(s) in "
            f"{negative.cycles_run} cycle(s) -> {'OK' if negative_ok else 'BLIND'}"
        )

        if positive_ok and negative_ok:
            print("SELFTEST OK")
            return 0
        if not positive_ok:
            print(
                f"SELFTEST FAILED: {positive.violations} violation(s) in the durable run "
                f"({positive.cycles_run}/{SELFTEST_CYCLES} cycles)",
                file=sys.stderr,
            )
        if not negative_ok:
            print("SELFTEST FAILED: the negative control found no violation", file=sys.stderr)
        return 1
    finally:
        shutil.rmtree(workdir, ignore_errors=True)


# -- argument parsing -----------------------------------------------------------------------


def add_common(parser: argparse.ArgumentParser) -> None:
    parser.add_argument("--seed", type=int, default=7, help="campaign seed")
    parser.add_argument("--driver", default="process-kill", help="cut driver name")
    parser.add_argument("--operations", type=int, default=2_000, help="workload operations per cycle")
    parser.add_argument("--workers", type=int, default=8, help="workload workers per cycle")
    parser.add_argument("--profile", default="smoke", help="workload profile name")
    parser.add_argument("--target", default="fake", choices=("fake", "local", "vm"))
    parser.add_argument("--fsync", default="on", choices=("on", "off"))
    parser.add_argument("--synchronous-commit", default="on", choices=("on", "off"))
    parser.add_argument("--full-page-writes", default="on", choices=("on", "off"))
    parser.add_argument(
        "--allow-unsafe-settings",
        action="store_true",
        help="skip the durability assertion (only for the negative control)",
    )
    parser.add_argument(
        "--confirm-disposable",
        action="store_true",
        help="required for --target local/vm: asserts the target is a throwaway machine",
    )
    parser.add_argument("--workload-cmd", default="")
    parser.add_argument("--snapshot-cmd", default="")
    parser.add_argument("--settings-cmd", default="")
    parser.add_argument("--health-cmd", default="")
    parser.add_argument("--pids-cmd", default="")
    parser.add_argument("--boot-cmd", default="")
    parser.add_argument("--guest-host", default="")
    parser.add_argument("--guest-user", default="")
    parser.add_argument("--vm", default="")
    parser.add_argument("--hypervisor", default="libvirt")
    parser.add_argument("--device", default="")


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(prog="powerloss.py", description=__doc__.splitlines()[0])
    sub = parser.add_subparsers(dest="command", required=True)

    sub.add_parser("drivers", help="list the cut drivers").set_defaults(fn=cmd_drivers)

    selftest = sub.add_parser("selftest", help="run 20 ProcessKill cycles and the negative control")
    add_common(selftest)
    selftest.set_defaults(fn=cmd_selftest)

    cycle = sub.add_parser("cycle", help="run exactly one cycle")
    add_common(cycle)
    cycle.add_argument("--cut-after", type=int, default=20)
    cycle.add_argument("--recovery-timeout-s", type=int, default=300)
    cycle.set_defaults(fn=cmd_cycle)

    camp = sub.add_parser("campaign", help="run N cycles, resumable")
    add_common(camp)
    camp.add_argument("--cycles", type=int, default=1_000)
    camp.add_argument("--cut-min-ops", type=int, default=1)
    camp.add_argument("--recovery-timeout-s", type=int, default=300)
    camp.add_argument("--out", default="")
    camp.set_defaults(fn=cmd_campaign)

    neg = sub.add_parser("negative-control", help="run with fsync=off and require a violation")
    add_common(neg)
    neg.add_argument("--cycles", type=int, default=1)
    neg.add_argument("--cut-min-ops", type=int, default=1)
    neg.add_argument("--recovery-timeout-s", type=int, default=300)
    neg.add_argument("--out", default="")
    neg.set_defaults(fn=cmd_negative_control)
    return parser


def main(argv: Sequence[str] | None = None) -> int:
    raw = list(sys.argv[1:] if argv is None else argv)
    # The brief's acceptance runs `powerloss.py --selftest`; accept it as a flag too.
    if "--selftest" in raw:
        raw = ["selftest"] + [token for token in raw if token != "--selftest"]
    parser = build_parser()
    args = parser.parse_args(raw)
    try:
        return args.fn(args)
    except (TargetError, cutdrivers.CutError) as exc:
        print(f"powerloss: {exc}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    sys.exit(main())
