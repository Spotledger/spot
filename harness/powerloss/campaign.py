"""Campaign mode: many cut cycles, resumable, with JSON-lines results and a summary.

A campaign is a pure list of cycles (each a seed and a cut offset, both derived from the
campaign seed), run one after another against one target. Each cycle's result is appended to
a JSON-lines file **and fsync'd before the next cycle starts**, so a campaign that is
interrupted (the operator stops it, the host reboots) resumes where it left off.

On the first invariant violation the campaign **stops** and records the failing cycle: its
seed, its cut offset, the cut driver and the report, so the run can be replayed and the
target's disk image kept for forensics. It never silently continues past a violation.
"""

from __future__ import annotations

import json
import os
from dataclasses import dataclass, field
from typing import Any

from checks import CheckReport
from l7history import COMMIT_CLASSES
from targets import CycleTarget, RecoveryInfo, run_cycle
from workload import Rng

RESULT_SCHEMA = "spot-powerloss-cycle/1"


@dataclass(frozen=True)
class CampaignConfig:
    """Everything a campaign needs."""

    campaign_seed: int
    cycles: int
    operations: int = 2_000
    workers: int = 8
    profile: str = "smoke"
    driver: str = "process-kill"
    cut_min_ops: int = 1
    recovery_timeout_us: int = 300_000_000
    enforce_settings: bool = True
    out_path: str = ""


def cycle_seeds(campaign_seed: int, cycles: int, operations: int, cut_min_ops: int) -> list[tuple[int, int]]:
    """``(seed, cut_after_ops)`` per cycle, a pure function of the campaign seed."""
    rng = Rng(campaign_seed)
    plan = []
    for _ in range(cycles):
        seed = rng.next_u64()
        low = max(1, min(cut_min_ops, operations))
        high = max(low, operations)
        cut = low + rng.below(high - low + 1)
        plan.append((seed, cut))
    return plan


@dataclass
class CycleOutcome:
    """What one cycle did."""

    cycle: int
    seed: int
    cut_after_ops: int
    driver: str
    recovery_us: int
    wal_replay_us: int
    invariants_ok: bool
    failed_ids: list[str]
    classification: dict[str, int]
    settings: dict[str, object]
    schema: str = RESULT_SCHEMA

    def to_json(self) -> dict[str, Any]:
        return {
            "schema": self.schema,
            "cycle": self.cycle,
            "seed": self.seed,
            "cut_after_ops": self.cut_after_ops,
            "driver": self.driver,
            "recovery_us": self.recovery_us,
            "wal_replay_us": self.wal_replay_us,
            "invariants_ok": self.invariants_ok,
            "failed_ids": self.failed_ids,
            "classification": self.classification,
            "settings": self.settings,
        }

    @staticmethod
    def from_json(raw: dict[str, Any]) -> "CycleOutcome":
        return CycleOutcome(
            cycle=int(raw["cycle"]),
            seed=int(raw["seed"]),
            cut_after_ops=int(raw["cut_after_ops"]),
            driver=str(raw["driver"]),
            recovery_us=int(raw["recovery_us"]),
            wal_replay_us=int(raw["wal_replay_us"]),
            invariants_ok=bool(raw["invariants_ok"]),
            failed_ids=[str(x) for x in raw.get("failed_ids", ())],
            classification={str(k): int(v) for k, v in raw.get("classification", {}).items()},
            settings=dict(raw.get("settings", {})),
        )


def outcome_from_report(
    cycle: int, seed: int, cut_after_ops: int, driver: str, recovery: RecoveryInfo, report: CheckReport, settings: dict[str, object]
) -> CycleOutcome:
    """Builds a cycle outcome from a check report."""
    return CycleOutcome(
        cycle=cycle,
        seed=seed,
        cut_after_ops=cut_after_ops,
        driver=driver,
        recovery_us=recovery.recovery_us,
        wal_replay_us=recovery.wal_replay_us,
        invariants_ok=report.ok,
        failed_ids=report.failed_ids(),
        classification=report.classification,
        settings=settings,
    )


def append_outcome(path: str, outcome: CycleOutcome) -> None:
    """Appends one result and fsyncs it, so the campaign can resume."""
    directory = os.path.dirname(os.path.abspath(path))
    os.makedirs(directory, exist_ok=True)
    fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_APPEND, 0o644)
    try:
        line = (json.dumps(outcome.to_json(), sort_keys=True) + "\n").encode("utf-8")
        os.write(fd, line)
        os.fsync(fd)
    finally:
        os.close(fd)


def load_outcomes(path: str) -> list[CycleOutcome]:
    """Reads the results already recorded, for a resume."""
    if not os.path.exists(path):
        return []
    outcomes = []
    with open(path, "r", encoding="utf-8") as handle:
        for line in handle:
            if line.strip():
                outcomes.append(CycleOutcome.from_json(json.loads(line)))
    return outcomes


@dataclass
class CampaignSummary:
    """The campaign's tally."""

    cycles_run: int
    violations: int
    stopped_on_violation: bool
    outcomes: list[CycleOutcome] = field(default_factory=list)

    def to_text(self) -> str:
        lines = ["cycle  seed                cut_ops  recovery_us  ok   failed"]
        for outcome in self.outcomes:
            failed = ",".join(outcome.failed_ids) if outcome.failed_ids else "-"
            lines.append(
                f"{outcome.cycle:>5}  {outcome.seed:<18}  {outcome.cut_after_ops:>7}  "
                f"{outcome.recovery_us:>11}  {'yes' if outcome.invariants_ok else 'NO ':>3}  {failed}"
            )
        lines.append("")
        lines.append(f"{self.cycles_run} cycles, {self.violations} violation(s)")
        if self.stopped_on_violation:
            lines.append("STOPPED on the first violation; the target is left as-is for forensics.")
        return "\n".join(lines)

    def classification_totals(self) -> dict[str, int]:
        totals = {name: 0 for name in COMMIT_CLASSES}
        for outcome in self.outcomes:
            for name, value in outcome.classification.items():
                totals[name] = totals.get(name, 0) + value
        return totals


def run_campaign(
    target: CycleTarget,
    config: CampaignConfig,
    *,
    os_name: str = "linux",
    on_cycle: Any = None,
) -> CampaignSummary:
    """Runs every cycle, resuming past the ones already recorded, stopping on a violation."""
    done = {outcome.cycle for outcome in load_outcomes(config.out_path)} if config.out_path else set()
    outcomes = load_outcomes(config.out_path) if config.out_path else []
    stopped = False
    for cycle, (seed, cut_after_ops) in enumerate(
        cycle_seeds(config.campaign_seed, config.cycles, config.operations, config.cut_min_ops)
    ):
        if cycle in done:
            continue
        report, recovery, settings = run_cycle(
            target,
            config.driver,
            seed=seed,
            cut_after_ops=cut_after_ops,
            recovery_timeout_us=config.recovery_timeout_us,
            enforce_settings=config.enforce_settings,
            os_name=os_name,
        )
        outcome = outcome_from_report(cycle, seed, cut_after_ops, config.driver, recovery, report, settings)
        outcomes.append(outcome)
        if config.out_path:
            append_outcome(config.out_path, outcome)
        if on_cycle is not None:
            on_cycle(outcome)
        if not outcome.invariants_ok:
            stopped = True
            break
    violations = sum(1 for outcome in outcomes if not outcome.invariants_ok)
    return CampaignSummary(
        cycles_run=len(outcomes),
        violations=violations,
        stopped_on_violation=stopped,
        outcomes=outcomes,
    )


def negative_control(target_factory: Any, config: CampaignConfig, *, os_name: str = "linux") -> CampaignSummary:
    """Runs one campaign with ``synchronous_commit`` off and requires the audit to fail.

    This proves the harness *can* fail. A negative control that finds no violation means the
    audit is blind and the campaign's green runs prove nothing.
    """
    target = target_factory()
    config = CampaignConfig(
        campaign_seed=config.campaign_seed,
        cycles=max(1, config.cycles),
        operations=config.operations,
        workers=config.workers,
        profile=config.profile,
        driver=config.driver,
        cut_min_ops=config.cut_min_ops,
        recovery_timeout_us=config.recovery_timeout_us,
        enforce_settings=False,
        out_path=config.out_path,
    )
    return run_campaign(target, config, os_name=os_name)
