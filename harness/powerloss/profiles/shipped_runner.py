"""The profile run loop: harness cycle plus the shipped-host assertions (WP-PRD-05.T2).

``targets.run_cycle`` is the harness's cycle. A profile adds two things to it: the settings
assertion is the **shipped** one (``SHOW`` values, ``shipped_settings``) and the invariant list
carries the profile extras (numbering, balances, jobs; ``shipped_extras``). The loop reuses the
campaign's seeds, append-and-fsync results file and resume, and stops on the first violation:
a lost acknowledged write is a release blocker, so the failing target is left as it is.
"""

from __future__ import annotations

import time
from dataclasses import dataclass
from typing import Callable

import campaign as campaign_mod
import shipped_extras
import shipped_settings
from checks import FRAMEWORK_INVARIANTS, CheckReport, InvariantResult, Violation, check_all
from cutdrivers import CutError, driver_by_name
from targets import CycleTarget, RecoveryInfo, TargetError

ExtrasReader = Callable[[], shipped_extras.ExtraSnapshot]


@dataclass(frozen=True)
class ProfileConfig:
    """Everything one profile run needs besides the target."""

    campaign: campaign_mod.CampaignConfig
    os_name: str
    wait_timeout_us: int = 300_000_000


def profile_cycle(
    target: CycleTarget,
    extras_reader: ExtrasReader,
    driver_name: str,
    *,
    seed: int,
    cut_after_ops: int,
    recovery_timeout_us: int,
    os_name: str,
    wait_timeout_us: int = 300_000_000,
) -> tuple[CheckReport, RecoveryInfo, dict[str, object]]:
    """One cycle against the shipped host: assert settings, load, cut, recover, judge."""
    target.prepare(seed)
    settings = target.settings()
    try:
        shipped_settings.assert_shipped(settings)
    except shipped_settings.ShippedSettingsError as exc:
        raise TargetError(str(exc)) from exc
    target.start_workload(seed, cut_after_ops)
    deadline = target.clock() + wait_timeout_us
    while target.workload_progress() < cut_after_ops:
        if target.clock() >= deadline:
            raise TargetError(f"workload did not reach the cut point ({cut_after_ops} operations) in time")
        time.sleep(0.05)
    driver = driver_by_name(driver_name)()
    if not driver.supported(os_name):
        raise CutError(f"driver {driver_name!r} does not support {os_name}")
    target.cut(driver_name)
    target.boot()
    recovery = target.wait_recovery(recovery_timeout_us)
    # The settings must still hold after recovery: a recovered server that came back with a
    # weaker configuration would invalidate every later cycle.
    after = target.settings()
    try:
        shipped_settings.assert_shipped(after)
    except shipped_settings.ShippedSettingsError as exc:
        raise TargetError("after recovery: " + str(exc)) from exc
    history = target.history()
    snapshot = target.snapshot()
    extras = extras_reader()
    invariants = tuple(FRAMEWORK_INVARIANTS) + shipped_extras.invariants(extras)
    report = check_all(history, snapshot, invariants=invariants, seed=seed)
    report = _with_snapshot_gaps(report, snapshot.missing)
    return report, recovery, settings


def _with_snapshot_gaps(report: CheckReport, missing: tuple[str, ...]) -> CheckReport:
    """A part of the store snapshot the reader could not read fails the cycle."""
    if not missing:
        return report
    found = [Violation("snapshot-complete", "snapshot", part, "part_unavailable") for part in missing]
    report.results.append(
        InvariantResult(
            id="snapshot-complete",
            describe="Every part of the recovered store could be read.",
            passed=False,
            total_violations=len(found),
            violations=found,
        )
    )
    report.ok = False
    return report


def run_profile(
    target: CycleTarget,
    extras_reader: ExtrasReader,
    driver_name: str,
    config: ProfileConfig,
    *,
    on_cycle: Callable[[campaign_mod.CycleOutcome], None] | None = None,
) -> campaign_mod.CampaignSummary:
    """Runs every cycle of the campaign through :func:`profile_cycle`, resuming and stopping on a violation."""
    camp = config.campaign
    outcomes = campaign_mod.load_outcomes(camp.out_path) if camp.out_path else []
    done = {outcome.cycle for outcome in outcomes}
    stopped = False
    for cycle, (seed, cut_after_ops) in enumerate(
        campaign_mod.cycle_seeds(camp.campaign_seed, camp.cycles, camp.operations, camp.cut_min_ops)
    ):
        if cycle in done:
            continue
        report, recovery, settings = profile_cycle(
            target,
            extras_reader,
            driver_name,
            seed=seed,
            cut_after_ops=cut_after_ops,
            recovery_timeout_us=camp.recovery_timeout_us,
            os_name=config.os_name,
            wait_timeout_us=config.wait_timeout_us,
        )
        outcome = campaign_mod.outcome_from_report(cycle, seed, cut_after_ops, driver_name, recovery, report, settings)
        outcomes.append(outcome)
        if camp.out_path:
            campaign_mod.append_outcome(camp.out_path, outcome)
        if on_cycle is not None:
            on_cycle(outcome)
        if not outcome.invariants_ok:
            stopped = True
            break
    violations = sum(1 for outcome in outcomes if not outcome.invariants_ok)
    return campaign_mod.CampaignSummary(
        cycles_run=len(outcomes), violations=violations, stopped_on_violation=stopped, outcomes=outcomes
    )
