#!/usr/bin/env python3
"""Run the power-loss harness against the shipped host (WP-PRD-05.T2).

    python3 harness/powerloss/profiles/run.py --dry-run
    python3 harness/powerloss/profiles/run.py run --profile linux-process-kill \\
        --confirm-disposable --pg-data /var/lib/spot/pgsql/data --root /srv/spot \\
        --cycles 200 --out results.jsonl
    python3 harness/powerloss/profiles/run.py run --profile windows-vm --confirm-disposable \\
        --vm windows-vm --guest-host HOST --guest-user USER --pg-data C:\\... --cycles 200

``--dry-run`` touches no machine: it prints each profile's plan (the adapters, the cut and the
boot commands), proves the settings assertion fails when a shipped value changes, runs a short
campaign on the model store through the same loop, and proves the profile's extra invariants
can fail. It exits non-zero when any of that is wrong.

A real run refuses to start without ``--confirm-disposable`` (the harness cannot tell a
throwaway machine from one that matters), asserts ``fsync``, ``full_page_writes`` and
``synchronous_commit`` from ``SHOW`` before **and after** every cut, and stops on the first
violation. A lost acknowledged write is a release blocker, not a flaky test.
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
HARNESS = os.path.dirname(HERE)
for path in (HERE, HARNESS):
    if path not in sys.path:
        sys.path.insert(0, path)

import campaign as campaign_mod  # noqa: E402
import cutdrivers  # noqa: E402
import powerloss  # noqa: E402
import shipped_extras  # noqa: E402
import shipped_profile as sp  # noqa: E402
import shipped_runner  # noqa: E402
import shipped_settings  # noqa: E402
import readers_dry_run  # noqa: E402
from targets import CommandTarget, FakeTarget, TargetError, ssh_runner, subprocess_runner  # noqa: E402
from workload import WorkloadConfig  # noqa: E402


# -- real target ----------------------------------------------------------------------------


def host_from_args(args: argparse.Namespace) -> sp.HostSettings:
    return sp.HostSettings(
        pg_data=args.pg_data,
        root=args.root,
        health_addr=args.health_addr,
        vm=args.vm,
        guest_host=args.guest_host,
        guest_user=args.guest_user,
        python=args.python,
        harness_dir=args.guest_harness_dir,
    )


def build_target(profile: sp.ShippedProfile, host: sp.HostSettings, history_path: str, environ) -> tuple[CommandTarget, shipped_runner.ExtrasReader]:
    guest = ssh_runner(host.guest_host, host.guest_user) if profile.ssh_guest else subprocess_runner
    target = CommandTarget(
        name=profile.name,
        runner=subprocess_runner,
        guest_runner=guest,
        clock=powerloss.monotonic_us,
        history_path=history_path,
        workload_argv=sp.workload_argv(profile, host, environ),
        snapshot_argv=sp.adapter_argv(profile, host, "snapshot"),
        settings_argv=sp.adapter_argv(profile, host, "settings"),
        health_argv=sp.adapter_argv(profile, host, "health"),
        pids_argv=sp.adapter_argv(profile, host, "pids"),
        boot_argv=sp.boot_argv(profile, host, environ),
        disposable=True,
        vm_name=host.vm,
    )
    extras_argv = sp.adapter_argv(profile, host, "extras")

    def read_extras() -> shipped_extras.ExtraSnapshot:
        try:
            return shipped_extras.ExtraSnapshot.from_json_text(target.guest_runner(extras_argv, 60.0))
        except shipped_extras.ExtrasError as exc:
            raise TargetError(str(exc)) from exc

    return target, read_extras


def cmd_run(args: argparse.Namespace) -> int:
    profile = sp.PROFILES.get(args.profile)
    if profile is None:
        raise sp.ProfileError(f"unknown profile {args.profile!r}; known: {', '.join(sorted(sp.PROFILES))}")
    if not args.confirm_disposable:
        raise TargetError(
            "refusing to cut a real target without --confirm-disposable: the harness cannot tell a "
            "throwaway machine from one you care about, so you must say it is one"
        )
    host = sp.validated(profile, host_from_args(args))
    os_name = "windows" if os.name == "nt" else "linux"
    if os_name != profile.os_name:
        raise sp.ProfileError(f"profile {profile.name} must run on a {profile.os_name} harness host, this is {os_name}")
    workdir = tempfile.mkdtemp(prefix="powerloss-shipped-")
    try:
        target, read_extras = build_target(profile, host, os.path.join(workdir, "history.jsonl"), os.environ)
        config = shipped_runner.ProfileConfig(
            campaign=campaign_mod.CampaignConfig(
                campaign_seed=args.seed,
                cycles=args.cycles,
                operations=args.operations,
                workers=args.workers,
                profile="smoke",
                driver=profile.driver,
                cut_min_ops=args.cut_min_ops,
                recovery_timeout_us=args.recovery_timeout_s * 1_000_000,
                enforce_settings=True,
                out_path=args.out or os.path.join(workdir, "results.jsonl"),
            ),
            os_name=os_name,
        )
        summary = shipped_runner.run_profile(target, read_extras, profile.driver, config)
        print(summary.to_text())
        print("classification totals:", json.dumps(summary.classification_totals(), sort_keys=True))
        if summary.violations:
            print("LOST OR BROKEN DATA: a release blocker, not a flaky test.", file=sys.stderr)
        return 0 if summary.violations == 0 and summary.cycles_run >= args.cycles else 1
    finally:
        shutil.rmtree(workdir, ignore_errors=True)


# -- dry run --------------------------------------------------------------------------------


class _PlanTarget:
    """A stand-in used only to print a driver's plan: it never runs anything."""

    def __init__(self, vm: str) -> None:
        self.vm_name = vm
        self.device = ""

    def is_disposable(self) -> bool:
        return True

    def service_pids(self) -> list[int]:
        return [1001, 1002]

    def execute(self, argv, *, timeout: float = 60.0) -> str:  # pragma: no cover - never called
        raise AssertionError("a dry run runs nothing")

    def simulate_cut(self, driver_name: str) -> None:  # pragma: no cover
        raise AssertionError("a dry run cuts nothing")


def _placeholder_host(profile: sp.ShippedProfile) -> sp.HostSettings:
    return sp.HostSettings(
        pg_data="<pg-data>",
        root="<root>",
        vm="vm-placeholder" if profile.target_kind == "vm" else "",
        guest_host="<guest-host>" if profile.ssh_guest else "",
        guest_user="<guest-user>" if profile.ssh_guest else "",
    )


def dry_run_plan(profile: sp.ShippedProfile) -> list[str]:
    """Human-readable plan of one profile; builds argv lists only."""
    host = _placeholder_host(profile)
    lines = [f"profile {profile.name}: {profile.description}"]
    lines.append(f"  harness host: {profile.os_name}; guest: {profile.guest_os}; driver: {profile.driver}; cycles: {profile.cycles}")
    lines.append("  host start: " + " ".join(sp.start_host_argv(profile, host)))
    for kind in ("settings", "health", "pids", "snapshot", "extras"):
        lines.append(f"  adapter {kind}: " + " ".join(sp.adapter_argv(profile, host, kind)))
    boot = sp.boot_argv(profile, host, {sp.START_CMD_ENV: "start-command"})
    lines.append("  boot: " + " ".join(boot))
    driver = cutdrivers.driver_by_name(profile.driver)
    if profile.driver == "vm-reset":
        driver_obj = driver(hypervisor=profile.hypervisor, vm=host.vm)
    else:
        driver_obj = driver()
    stub = _PlanTarget(host.vm)
    if profile.os_name == "windows" and profile.driver == "process-kill":
        raise sp.ProfileError("no windows process-kill profile")
    for argv in driver_obj.plan(stub):
        lines.append("  cut: " + " ".join(argv))
    lines.append("  asserts before and after every cut: " + ", ".join(f"{n}={v}" for n, v in shipped_settings.REQUIRED))
    return lines


def _model_target(workdir: str, *, fsync: bool = True, sync: bool = True, name: str = "h") -> FakeTarget:
    return FakeTarget(
        durability={"fsync": fsync, "synchronous_commit": sync, "full_page_writes": True},
        config=WorkloadConfig(seed=0, workers=2, operations=40, profile="smoke"),
        history_path=os.path.join(workdir, f"{name}.jsonl"),
        clock=powerloss.monotonic_us,
    )


def _model_config(workdir: str, cycles: int, seed: int, out: str = "dry.jsonl") -> shipped_runner.ProfileConfig:
    return shipped_runner.ProfileConfig(
        campaign=campaign_mod.CampaignConfig(
            campaign_seed=seed,
            cycles=cycles,
            operations=40,
            workers=2,
            profile="smoke",
            driver="process-kill",
            cut_min_ops=1,
            recovery_timeout_us=30_000_000,
            enforce_settings=True,
            out_path=os.path.join(workdir, out),
        ),
        os_name="linux",
    )


def _clean_extras() -> shipped_extras.ExtraSnapshot:
    return shipped_extras.ExtraSnapshot(
        numbers={"SINV": ("SINV-0001", "SINV-0002")},
        ledger={"PKR": (100_00, 100_00)},
        job_runs={"nightly:1": 1},
    )


def dry_run(seed: int) -> int:
    problems: list[str] = []
    for name in sorted(sp.PROFILES):
        try:
            for line in dry_run_plan(sp.PROFILES[name]):
                print(line)
        except (sp.ProfileError, cutdrivers.CutError) as exc:
            problems.append(f"{name}: {exc}")

    # 1. The shipped values pass; every changed value fails.
    try:
        shipped_settings.assert_shipped({"fsync": "on", "full_page_writes": "on", "synchronous_commit": "on"})
    except shipped_settings.ShippedSettingsError as exc:
        problems.append(f"shipped values rejected: {exc}")
    for name, bad in (("fsync", "off"), ("full_page_writes", "off"), ("synchronous_commit", "off"), ("synchronous_commit", "local")):
        settings = {"fsync": "on", "full_page_writes": "on", "synchronous_commit": "on", name: bad}
        if not shipped_settings.problems(settings):
            problems.append(f"{name}={bad} was not rejected")
    print("settings assertion: shipped values accepted, changed values rejected")

    workdir = tempfile.mkdtemp(prefix="powerloss-shipped-dry-")
    try:
        # 2. A short campaign on the model store, through the profile loop, passes.
        summary = shipped_runner.run_profile(
            _model_target(workdir), _clean_extras, "process-kill", _model_config(workdir, sp.DRY_RUN_CYCLES, seed)
        )
        print(f"model campaign through the profile loop: {summary.cycles_run} cycles, {summary.violations} violation(s)")
        if summary.violations or summary.cycles_run != sp.DRY_RUN_CYCLES:
            problems.append(f"model campaign: {summary.violations} violation(s) in {summary.cycles_run} cycles")

        # 3. A changed durability setting stops the run before any cut.
        try:
            shipped_runner.run_profile(
                _model_target(workdir, fsync=False, name="off"), _clean_extras, "process-kill", _model_config(workdir, 1, seed, "off.jsonl")
            )
            problems.append("fsync=off was not refused by the profile loop")
        except TargetError:
            print("fsync=off: refused before the first cut")

        # 4. The profile extras can fail (an audit that cannot fail proves nothing).
        broken = shipped_extras.ExtraSnapshot(
            numbers={"SINV": ("SINV-0001", "SINV-0001")},
            ledger={"PKR": (100, 99)},
            job_runs={"nightly:1": 2},
        )
        found = (
            shipped_extras.numbering_violations(broken)
            + shipped_extras.balance_violations(broken)
            + shipped_extras.job_violations(broken)
        )
        if len(found) != 3:
            problems.append(f"profile extras found {len(found)} of 3 planted violations")
        print(f"profile extras: {len(found)} of 3 planted violations found")
        # 5. The operator pieces (post client, snapshot reader, extras reader) against a fake API.
        problems.extend(readers_dry_run.exercise(workdir, seed))
    finally:
        shutil.rmtree(workdir, ignore_errors=True)

    if problems:
        for problem in problems:
            print(f"DRY RUN FAILED: {problem}", file=sys.stderr)
        return 1
    print("DRY RUN OK")
    return 0


# -- arguments ------------------------------------------------------------------------------


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(prog="run.py", description=__doc__.splitlines()[0])
    parser.add_argument("--dry-run", action="store_true", help="print the plans and run the self-checks; touches no machine")
    parser.add_argument("--seed", type=int, default=7)
    sub = parser.add_subparsers(dest="command")
    run = sub.add_parser("run", help="run one profile against a real disposable target")
    run.add_argument("--profile", required=True, choices=sorted(sp.PROFILES))
    run.add_argument("--confirm-disposable", action="store_true")
    run.add_argument("--cycles", type=int, default=sp.ACCEPTANCE_CYCLES)
    run.add_argument("--seed", type=int, default=7)
    run.add_argument("--operations", type=int, default=2_000)
    run.add_argument("--workers", type=int, default=8)
    run.add_argument("--cut-min-ops", type=int, default=1)
    run.add_argument("--recovery-timeout-s", type=int, default=300)
    run.add_argument("--out", default="")
    run.add_argument("--pg-data", default="")
    run.add_argument("--root", default="")
    run.add_argument("--health-addr", default=sp.DEFAULT_HEALTH_ADDR)
    run.add_argument("--vm", default=os.environ.get("SPOT_POWERLOSS_VM", ""))
    run.add_argument("--guest-host", default=os.environ.get("SPOT_POWERLOSS_GUEST_HOST", ""))
    run.add_argument("--guest-user", default=os.environ.get("SPOT_POWERLOSS_GUEST_USER", ""))
    run.add_argument("--python", default="")
    run.add_argument("--guest-harness-dir", default="", help="where harness/powerloss lives on the guest")
    run.set_defaults(fn=cmd_run)
    return parser


def main(argv: Sequence[str] | None = None) -> int:
    parser = build_parser()
    args = parser.parse_args(list(sys.argv[1:] if argv is None else argv))
    if args.dry_run:
        return dry_run(args.seed)
    if not getattr(args, "fn", None):
        parser.error("give a subcommand (run) or --dry-run")
    try:
        return args.fn(args)
    except (TargetError, cutdrivers.CutError, sp.ProfileError) as exc:
        print(f"run.py: {exc}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    sys.exit(main())
