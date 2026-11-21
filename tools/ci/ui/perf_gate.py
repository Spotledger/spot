#!/usr/bin/env python3
"""Aggregate UI performance evidence. Missing/invalid evidence always fails.

No PR write token: the review table is appended to GITHUB_STEP_SUMMARY. Runner
identity/kind and expected commit come from the invocation, never result claims.
"""
from __future__ import annotations

import argparse
from dataclasses import dataclass
import html
import json
import math
import os
from pathlib import Path
import re
import sys
import tomllib


class GateError(ValueError):
    """Invalid configuration or evidence; not a performance warning."""


def positive(value):
    if type(value) not in (float, int) or not math.isfinite(value) or value <= 0:
        raise GateError(f'Expected positive finite number, got {value!r}')
    return value


@dataclass(frozen=True)
class Declaration:
    id: str
    metric: str
    max: float
    unit: str
    runner: str
    multipliers: dict

    def multiplier(self, label, kind):
        if kind == 'reference':
            if label != 'reference':
                raise GateError('Reference kind requires the reference label')
            return 1
        if label == 'reference':
            raise GateError('Reference label requires verified reference kind')
        return 1 if kind == 'hosted' else self.multipliers.get(label, 3)


def load_budgets(paths):
    declarations = {}
    for path in paths:
        try:
            data = tomllib.loads(Path(path).read_text(encoding='utf-8'))
        except (OSError, ValueError) as error:
            raise GateError(f'{path}: {error}') from error
        runners = data.pop('runner', {})
        if not isinstance(runners, dict):
            raise GateError(f'{path}: runner must be a table')
        multipliers = {}
        for label, entry in runners.items():
            if not re.fullmatch(r'[A-Za-z0-9_-]+', label) or not isinstance(entry, dict) or set(entry) != {'multiplier'}:
                raise GateError(f'{path}: invalid runner table {label}')
            value = positive(entry['multiplier'])
            if value < 1 or (label == 'reference' and value != 1):
                raise GateError('Multipliers must be >= 1; reference must be 1')
            multipliers[label] = value
        if 'budget' in data:
            if set(data) != {'budget'} or not isinstance(data['budget'], list):
                raise GateError(f'{path}: expected only [[budget]] and [runner.<label>]')
            entries = data['budget']
        else:
            legacy = {'max_frame_ms', 'max_alloc_bytes_per_frame', 'first_paint_50_rows_ms', 'rows', 'columns', 'scroll_steps'}
            if set(data) != legacy:
                raise GateError(f'{path}: no budgets or invalid legacy keys')
            for key in ('rows', 'columns', 'scroll_steps'):
                if type(data[key]) is not int or data[key] <= 0:
                    raise GateError(f'{path}: invalid workload {key}')
            entries = [dict(id=id_, metric=metric, max=data[key], unit=unit, runner='reference')
                       for id_, metric, key, unit in (
                           ('grid.frame.max', 'max', 'max_frame_ms', 'ms'),
                           ('grid.frame.alloc.mean', 'mean', 'max_alloc_bytes_per_frame', 'bytes'),
                           ('grid.first-paint', 'p95', 'first_paint_50_rows_ms', 'ms'))]
        if not entries:
            raise GateError(f'{path}: no budgets')
        for entry in entries:
            if not isinstance(entry, dict) or set(entry) != {'id', 'metric', 'max', 'unit', 'runner'}:
                raise GateError(f'{path}: invalid budget fields')
            id_ = entry['id']
            if not isinstance(id_, str) or not re.fullmatch(r'[A-Za-z0-9][A-Za-z0-9._-]*', id_):
                raise GateError(f'{path}: invalid budget id')
            if entry['metric'] not in ('p50', 'p95', 'max', 'mean') or entry['unit'] not in ('ms', 'bytes') or entry['runner'] not in ('reference', 'ci-linux', 'ci-windows'):
                raise GateError(f'{path}: invalid budget vocabulary')
            if id_ in declarations:
                raise GateError(f'Duplicate budget id {id_}')
            declarations[id_] = Declaration(id_, entry['metric'], positive(entry['max']), entry['unit'], entry['runner'], multipliers)
    if not declarations:
        raise GateError('No budget files')
    return declarations


@dataclass(frozen=True)
class Outcome:
    ok: bool
    markdown: str


def no_duplicates(pairs):
    result = {}
    for key, value in pairs:
        if key in result:
            raise GateError(f'Duplicate JSON key {key}')
        result[key] = value
    return result


def evaluate(paths, results, runner, kind, commit, advisory=False):
    if kind not in ('hosted', 'self-hosted', 'reference') or not runner or not commit:
        raise GateError('Expected runner, kind and commit')
    budgets = load_budgets(paths)
    active = {id_: budget for id_, budget in budgets.items() if budget.runner in ('reference', runner)}
    if not active:
        raise GateError('No active budgets for this OS')
    if not Path(results).is_dir():
        raise GateError(f'Result directory missing: {results}')
    seen = {}
    for path in sorted(Path(results).rglob('*.json')):
        try:
            record = json.loads(path.read_text(encoding='utf-8'), object_pairs_hook=no_duplicates)
        except (OSError, ValueError) as error:
            raise GateError(f'{path}: malformed JSON: {error}') from error
        required = {'id', 'measured', 'max', 'runner', 'commit', 'metric', 'unit'}
        if not isinstance(record, dict) or not required <= record.keys():
            raise GateError(f'{path}: missing result fields')
        id_ = record['id']
        if not isinstance(id_, str) or id_ not in active:
            raise GateError(f'{path}: unknown/inactive budget id {id_!r}')
        budget = active[id_]
        for key, expected in (('max', budget.max), ('metric', budget.metric), ('unit', budget.unit), ('runner', runner), ('commit', commit)):
            if record[key] != expected or (key == 'max' and type(record[key]) not in (float, int)):
                raise GateError(f'{path}: {key} differs from expected {expected!r}')
        value = record['measured']
        if type(value) not in (float, int) or not math.isfinite(value) or value < 0:
            raise GateError(f'{path}: measured must be finite and nonnegative')
        # Multiple executions are allowed, but no later success masks a breach.
        seen[id_] = max(seen.get(id_, 0), value)
    lines = ['## UI performance gate', '',
             f'**APPLIED MULTIPLIER: runner `{runner}`, kind `{kind}`.** '
             'Per-budget factors below; multiplied breaches are WARNING until reference hardware is reviewed.'
             + (' **ADVISORY: every breach is a WARNING on this runner (SPOT_PERF_ADVISORY=1); missing results still fail.**' if advisory else ''), '',
             '| Budget | Measured | Base | Multiplier | Allowed | Unit | Status |',
             '| --- | ---: | ---: | ---: | ---: | --- | --- |']
    ok = True
    for id_, budget in sorted(active.items()):
        multiplier = budget.multiplier(runner, kind)
        allowed = budget.max * multiplier
        if not math.isfinite(allowed):
            raise GateError('Scaled budget overflow')
        measured = seen.get(id_)
        if measured is None:
            status = 'MISSING'
        elif measured <= allowed:
            status = 'PASS'
        else:
            status = 'WARNING' if multiplier > 1 or advisory else 'FAIL'
        ok = ok and status not in ('MISSING', 'FAIL')
        text = '—' if measured is None else f'{measured:.6g}'
        lines.append(f'| {id_} | {text} | {budget.max:g} | **{multiplier:g}x** | {allowed:g} | {budget.unit} | {status} |')
    return Outcome(ok, '\n'.join(lines) + '\n')


def discover_budgets(root):
    root = Path(root)
    paths = sorted(path for path in root.rglob('perf-budgets.toml')
                   if not {'bin', 'obj'} & set(path.relative_to(root).parts))
    if not paths:
        raise GateError(f'No budget files beneath {root}')
    return paths


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    source = parser.add_mutually_exclusive_group(required=True)
    source.add_argument('--budget', action='append', type=Path)
    source.add_argument('--budget-root', type=Path)
    parser.add_argument('--results', required=True, type=Path)
    parser.add_argument('--runner', default=os.environ.get('SPOT_PERF_RUNNER', 'ci-linux'))
    parser.add_argument('--kind', default=os.environ.get('SPOT_RUNNER_KIND', 'self-hosted'))
    parser.add_argument('--commit', default=os.environ.get('GITHUB_SHA', 'local'))
    parser.add_argument('--advisory', action='store_true', default=os.environ.get('SPOT_PERF_ADVISORY') == '1',
                        help='record every budget breach as WARNING instead of failing (missing results still fail)')
    args = parser.parse_args(argv)
    try:
        paths = discover_budgets(args.budget_root) if args.budget_root else args.budget
        outcome = evaluate(paths, args.results, args.runner, args.kind, args.commit, args.advisory)
    except (GateError, OverflowError) as error:
        outcome = Outcome(False, f'## UI performance gate\n\n**ERROR:** {html.escape(str(error))}\n')
    print(outcome.markdown)
    summary = os.environ.get('GITHUB_STEP_SUMMARY')
    if summary:
        with open(summary, 'a', encoding='utf-8') as handle:
            handle.write('\n' + outcome.markdown)
    return 0 if outcome.ok else 1


if __name__ == '__main__':
    sys.exit(main())
