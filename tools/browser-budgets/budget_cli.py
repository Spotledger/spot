#!/usr/bin/env python3
"""Command line for the browser budgets: validate, project, plan, aggregate, check.

`run.sh` is a thin wrapper around this; keeping the arithmetic and the verdicts
here means every rule is covered by a unit test that needs no browser.

    budget_cli.py validate  [--budgets budgets.toml]
    budget_cli.py gate      [--budgets budgets.toml] [--out perf-budgets.toml] [--write]
    budget_cli.py plan      --out raw/plan.json [--mode quick|full] [--runs N]
    budget_cli.py aggregate --raw DIR --out DIR --runner LABEL --commit SHA
    budget_cli.py check     --results DIR --runner LABEL --commit SHA

Exit status: 0 success, 1 a breach, a missing result or invalid evidence,
2 a usage or catalogue error.
"""

from __future__ import annotations

import argparse
import json
import os
import sys
from pathlib import Path

import budgets as catalogue_module

HERE = Path(__file__).resolve().parent
DEFAULT_BUDGETS = HERE / "budgets.toml"
DEFAULT_GATE = HERE / "perf-budgets.toml"


def _load(path) -> catalogue_module.Catalogue:
    return catalogue_module.load(path)


def _amount(value) -> str:
    """Readable maximum: thousands separators, no exponent for whole numbers."""
    return f"{value:,}" if type(value) is int else f"{value:g}"


def _summary(catalogue) -> str:
    proposed = [budget.id for budget in catalogue.budgets if budget.proposed]
    quick = [budget for budget in catalogue.budgets if budget.quick]
    lines = [f"{len(catalogue.budgets)} budgets: {len(quick)} measured by --quick, "
             f"{len(catalogue.budgets) - len(quick)} by --full only; "
             f"{len(proposed)} proposed"]
    for budget in catalogue.budgets:
        lines.append(f"  {budget.id:34s} {budget.metric:>4s} <= {_amount(budget.max):>12s} "
                     f"{budget.unit:<5s} quick={str(budget.quick).lower():<5s} "
                     f"profile={budget.profile}"
                     f"{'  proposed' if budget.proposed else ''}")
    return "\n".join(lines)


def command_validate(args) -> int:
    catalogue = _load(args.budgets)
    print(f"{args.budgets}: valid")
    print(_summary(catalogue))
    return 0


def command_gate(args) -> int:
    catalogue = _load(args.budgets)
    out = Path(args.out) if args.out else DEFAULT_GATE
    if args.write:
        changed = catalogue_module.write_gate_catalogue(catalogue, out)
        print(f"{out}: {'written' if changed else 'already up to date'}")
        return 0
    expected = catalogue_module.gate_text(catalogue)
    try:
        current = out.read_text(encoding="utf-8")
    except OSError:
        current = None
    if current != expected:
        print(f"{out}: differs from the projection of {args.budgets}\n"
              f"regenerate with: python3 {Path(__file__).name} gate --write", file=sys.stderr)
        return 1
    print(f"{out}: matches the projection of {args.budgets}")
    return 0


def command_plan(args) -> int:
    catalogue = _load(args.budgets)
    selected = catalogue.quick() if args.mode == "quick" else catalogue.budgets
    if not selected:
        print(f"{args.budgets}: no budget is measured by --quick", file=sys.stderr)
        return 1
    plan = {
        "mode": args.mode,
        "runs": args.runs,
        "budgets": [
            {"id": budget.id, "metric": budget.metric, "unit": budget.unit,
             "max": budget.max, "profile": budget.profile}
            for budget in selected
        ],
    }
    text = json.dumps(plan, indent=2, sort_keys=True) + "\n"
    if args.out:
        out = Path(args.out)
        out.parent.mkdir(parents=True, exist_ok=True)
        out.write_text(text, encoding="utf-8")
        print(f"{out}: {len(selected)} measure(s), {args.runs} run(s), mode {args.mode}")
    else:
        sys.stdout.write(text)
    return 0


def command_aggregate(args) -> int:
    catalogue = _load(args.budgets)
    try:
        runs = catalogue_module.read_runs(args.raw)
    except catalogue_module.BudgetError as error:
        print(f"aggregate: {error}", file=sys.stderr)
        return 1
    if args.runs is not None and len(runs) != args.runs:
        print(f"aggregate: expected {args.runs} raw run(s), found {len(runs)}: "
              f"{[run.get('run') for run in runs]}", file=sys.stderr)
        return 1
    extra = {"fixture": True} if any(run.get("fixture") for run in runs) else None
    try:
        verdict = catalogue_module.aggregate(catalogue, runs, runner=args.runner,
                                            commit=args.commit, extra=extra)
    except catalogue_module.BudgetError as error:
        print(f"aggregate: {error}", file=sys.stderr)
        return 1
    for record in verdict.records:
        stats = record.get("stats") or {}
        suffix = "  (informational)" if record.get("informational") else ""
        print(f"  {record['status']:6s} {record['id']:34s} "
              f"{record['metric']} {_amount(record['measured']):>12s} {record['unit']:<5s} "
              f"<= {_amount(record['max']):>12s}  n={record['count']:<4d} "
              f"p50={_amount(stats.get('p50', record['measured']))} "
              f"p95={_amount(stats.get('p95', record['measured']))}{suffix}")
    for id_, why in verdict.missing:
        print(f"  MISSING {id_}: {why}", file=sys.stderr)
    written = catalogue_module.write_results(Path(args.out), verdict)
    print(f"{args.out}: {len(written)} result file(s)")
    if not verdict.ok:
        print(f"aggregate: FAILED: {verdict.reason()}", file=sys.stderr)
        return 1
    return 0


def command_check(args) -> int:
    catalogue = _load(args.budgets)
    problems = catalogue_module.check_results(Path(args.results), catalogue,
                                             runner=args.runner, commit=args.commit)
    if problems:
        for problem in problems:
            print(f"check: {problem}", file=sys.stderr)
        return 1
    print(f"{args.results}: {len(catalogue.budgets)} budget result(s) valid, no breach")
    return 0


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__,
                                     formatter_class=argparse.RawDescriptionHelpFormatter)
    subparsers = parser.add_subparsers(dest="command", required=True)

    def add_common(sub, name, help_text):
        sub.add_argument("--budgets", type=Path, default=DEFAULT_BUDGETS, help=help_text)
        sub.set_defaults(handler=name)

    validate = subparsers.add_parser("validate", help="validate the catalogue")
    add_common(validate, command_validate, "catalogue to validate")

    gate = subparsers.add_parser("gate", help="check or write the gate projection")
    add_common(gate, command_gate, "catalogue to project")
    gate.add_argument("--out", type=Path, help="projection path (default: perf-budgets.toml)")
    gate.add_argument("--write", action="store_true", help="write the projection")

    plan = subparsers.add_parser("plan", help="write the driver's plan")
    add_common(plan, command_plan, "catalogue to plan from")
    plan.add_argument("--mode", choices=("quick", "full"), default="quick")
    plan.add_argument("--runs", type=int, default=1)
    plan.add_argument("--out", type=Path)

    aggregate = subparsers.add_parser("aggregate", help="aggregate raw runs into result files")
    add_common(aggregate, command_aggregate, "catalogue to aggregate against")
    aggregate.add_argument("--raw", type=Path, required=True)
    aggregate.add_argument("--out", type=Path, required=True)
    aggregate.add_argument("--runs", type=int)
    aggregate.add_argument("--runner", default=os.environ.get("SPOT_PERF_RUNNER", "ci-linux"))
    aggregate.add_argument("--commit", default=os.environ.get("GITHUB_SHA", "local"))

    check = subparsers.add_parser("check", help="validate a result directory")
    add_common(check, command_check, "catalogue the results must match")
    check.add_argument("--results", type=Path, required=True)
    check.add_argument("--runner", default=os.environ.get("SPOT_PERF_RUNNER", "ci-linux"))
    check.add_argument("--commit", default=os.environ.get("GITHUB_SHA", "local"))
    return parser


def main(argv=None) -> int:
    parser = build_parser()
    args = parser.parse_args(argv)
    try:
        return args.handler(args)
    except catalogue_module.BudgetError as error:
        print(f"{args.command}: {error}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    sys.exit(main())
