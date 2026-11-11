#!/usr/bin/env python3
"""Run the L7 and L8 `nightly` profiles and fail on any violation (WP-TST-08.T3).

The nightly workflow runs `cargo test --workspace -- --ignored`, but the L7 nightly profile
(200 workers, 1M operations), the L8 fault scenarios and the L8 isolation matrix at the `nightly`
profile are selected by environment variables, so the scheduled run never reaches them.
This script runs them with `SPOT_REQUIRE_PG=1` (a missing PostgreSQL is a failure, never a skip),
seeds them from the run id, writes every report and the seed under the output directory, reads the
reports back and exits 1 if any step failed or any report says `ok: false`.

Time is an argument: nothing here reads the clock. The L8 fault scenarios use seeds fixed in the
test source; their reports record the seed they used, and `summary.json` records the seed given
here for the L7 and isolation runs, so a red night can be replayed.

    nightly_profiles.py --run-id 123456 [--out target/nightly-profiles] [--seeds 5] [--dry-run]
"""
from __future__ import annotations

import argparse
import json
import os
import subprocess
import sys
from dataclasses import dataclass, field
from pathlib import Path
from typing import Mapping, Sequence

DEFAULT_OUT = "target/nightly-profiles"
DEFAULT_SEEDS = 5
WORKLOADS = ("list-append", "rw-register")
DEFAULT_SEED = 20_261_017  # the L7 default when no run id is available


@dataclass(frozen=True)
class Step:
    name: str
    argv: Sequence[str]
    env: Mapping[str, str]
    reports: Sequence[Path] = field(default_factory=tuple)  # files that must exist and say ok


def parse_args(argv: Sequence[str], environ: Mapping[str, str]) -> argparse.Namespace:
    p = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    p.add_argument("--run-id", default=environ.get("GITHUB_RUN_ID"), help="seed source (default $GITHUB_RUN_ID)")
    p.add_argument("--seed", type=int, default=None, help="explicit base seed; overrides --run-id")
    p.add_argument("--out", default=DEFAULT_OUT, help=f"output directory (default {DEFAULT_OUT})")
    p.add_argument("--seeds", type=int, default=DEFAULT_SEEDS, help="isolation seeds per workload")
    p.add_argument("--dry-run", action="store_true", help="print the plan and run nothing")
    args = p.parse_args(argv)
    if args.seeds < 1:
        p.error("--seeds must be at least 1")
    args.base_seed = resolve_seed(args.seed, args.run_id, p)
    return args


def resolve_seed(seed: int | None, run_id: str | None, parser: argparse.ArgumentParser) -> int:
    if seed is not None:
        if seed < 0:
            parser.error("--seed must not be negative")
        return seed
    if run_id is None or run_id == "":
        return DEFAULT_SEED
    if not run_id.isdigit():
        parser.error(f"run id must be a number, got {run_id!r}")
    return int(run_id)


def build_steps(base_seed: int, out: Path, seeds: int) -> list[Step]:
    l7_dir = out / "l7"
    faults_dir = out / "l8-faults"
    iso_dir = out / "l8-isolation"
    steps = [
        Step(
            "l7-engine-nightly",
            ["cargo", "test", "-p", "spot-l7-engine", "--test", "profiles", "--", "--ignored", "--nocapture"],
            {"SPOT_L7_PROFILE": "nightly", "SPOT_L7_SEED": str(base_seed), "SPOT_L7_REPORT_DIR": str(l7_dir)},
            [l7_dir / "engine-pg-nightly.json"],
        ),
        Step(
            "l8-faults-nightly",
            ["cargo", "test", "-p", "spot-l8-faults", "--", "--ignored", "--nocapture", "--test-threads=1"],
            {"SPOT_L8_FAULTS_PROFILE": "nightly", "SPOT_L8_FAULTS_OUT": str(faults_dir)},
        ),
    ]
    for workload in WORKLOADS:
        for i in range(seeds):
            seed = base_seed + i
            report = iso_dir / f"matrix-{workload}-{seed}.json"
            steps.append(
                Step(
                    f"l8-isolation-{workload}-{seed}",
                    [
                        "cargo", "run", "--quiet", "-p", "spot-l8-isolation", "--bin", "spot-l8", "--",
                        "matrix", "--profile", "nightly", "--workload", workload,
                        "--seed", str(seed), "--report", str(report),
                    ],
                    {},
                    [report],
                )
            )
    return steps


def report_problems(path: Path) -> list[str]:
    """Problems found in one JSON report; empty when it says the run was clean.

    L7 and L8 fault reports carry a top-level `ok`. The isolation matrix report has no `ok`
    (the `spot-l8 matrix` exit status is its verdict), so it only has to exist and parse.
    """
    try:
        data = json.loads(path.read_text(encoding="utf-8"))
    except FileNotFoundError:
        return [f"{path}: report missing"]
    except (OSError, ValueError) as e:
        return [f"{path}: unreadable report ({e})"]
    if not isinstance(data, dict):
        return [f"{path}: report is not a JSON object"]
    if data.get("ok") is False:
        detail = data.get("violations") or [r.get("id") for r in data.get("results", []) if not r.get("passed", True)]
        return [f"{path}: violations {detail}"]
    return []


def scan_reports(directory: Path) -> list[str]:
    """Problems in every `*.json` report a step wrote into `directory` (fault scenario reports)."""
    problems: list[str] = []
    for f in sorted(directory.glob("*.json")):
        problems.extend(report_problems(f))
    return problems


def run_step(step: Step, environ: Mapping[str, str]) -> list[str]:
    env = dict(environ)
    env.update(step.env)
    env["SPOT_REQUIRE_PG"] = "1"
    for key, path in step.env.items():
        if key.endswith(("_DIR", "_OUT")):
            Path(path).mkdir(parents=True, exist_ok=True)
    for r in step.reports:
        r.parent.mkdir(parents=True, exist_ok=True)
    print(f"== {step.name}: {' '.join(step.argv)}", flush=True)
    code = subprocess.run(list(step.argv), env=env, check=False).returncode
    problems = [f"{step.name}: exited {code}"] if code != 0 else []
    for r in step.reports:
        problems.extend(report_problems(r))
    return problems


def main(argv: Sequence[str] | None = None, environ: Mapping[str, str] | None = None) -> int:
    environ = os.environ if environ is None else environ
    args = parse_args(sys.argv[1:] if argv is None else argv, environ)
    out = Path(args.out)
    steps = build_steps(args.base_seed, out, args.seeds)
    if args.dry_run:
        print(f"seed={args.base_seed} out={out}")
        for s in steps:
            print(f"{s.name}: {' '.join(s.argv)}")
        return 0
    out.mkdir(parents=True, exist_ok=True)
    (out / "seed.txt").write_text(f"{args.base_seed}\n", encoding="utf-8")
    problems: list[str] = []
    for s in steps:
        problems.extend(run_step(s, environ))
    for sub in ("l7", "l8-faults"):
        if (out / sub).is_dir():
            problems.extend(scan_reports(out / sub))
    problems = list(dict.fromkeys(problems))
    summary = {"seed": args.base_seed, "ok": not problems, "problems": problems}
    (out / "summary.json").write_text(json.dumps(summary, indent=2) + "\n", encoding="utf-8")
    for p in problems:
        print(f"NIGHTLY VIOLATION: {p}", file=sys.stderr)
    print(f"nightly profiles: {'OK' if not problems else 'FAILED'} (seed {args.base_seed}, reports in {out})")
    return 1 if problems else 0


if __name__ == "__main__":
    sys.exit(main())
