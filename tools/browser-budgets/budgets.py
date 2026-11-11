#!/usr/bin/env python3
"""Browser budget catalogue, gate projection and result aggregation.

Standard library only: no network, no browser, no .NET. `run.sh` uses this
module to validate `budgets.toml`, to plan a measurement, to aggregate the raw
per-run samples the Playwright driver writes, and to produce one result file
per budget in the format `tools/ci/ui/perf_gate.py` reads.

The catalogue is deliberately small and strict: unknown keys, a missing
reference-hardware comment, a negative or non-finite maximum, a duplicate id or
an unknown vocabulary word are errors, never warnings.
"""

from __future__ import annotations

import dataclasses
import json
import math
import re
import tomllib
from pathlib import Path

BUDGET_ID = re.compile(r"[A-Za-z0-9][A-Za-z0-9._-]*")
RUNNER_LABEL = re.compile(r"[A-Za-z0-9_-]+")
REFERENCE_COMMENT = re.compile(r"^#\s*reference\s*:\s*(\S.*)$", re.IGNORECASE)

METRICS = ("p50", "p95", "max", "mean")
UNITS = ("ms", "bytes")
RUNNERS = ("reference", "ci-linux", "ci-windows")
PROFILES = ("fifty", "fast3g", "local")

REQUIRED_KEYS = ("id", "metric", "max", "unit", "quick", "runner", "profile")
OPTIONAL_KEYS = ("proposed", "note", "informational", "informational_reason")
REFERENCE_KEYS = ("hardware", "network", "browser")

#: The keys `tools/ci/ui/perf_gate.py` accepts in a `[[budget]]` entry. The
#: projection written for the gate carries exactly these and nothing else.
GATE_KEYS = ("id", "metric", "max", "unit", "runner")

RESULT_REQUIRED = ("id", "measured", "max", "runner", "commit", "metric", "unit")


class BudgetError(ValueError):
    """Invalid catalogue, raw samples or result evidence."""


@dataclasses.dataclass(frozen=True)
class Budget:
    id: str
    metric: str
    max: float
    unit: str
    quick: bool
    runner: str
    profile: str
    proposed: bool = False
    note: str = ""
    #: An informational budget's breach is reported but never fails the exit
    #: status; a MISSING result for it still fails (absence is never a pass).
    #: `informational_reason` names who decides and since when, and is
    #: mandatory whenever `informational` is true (see `_parse_budget`).
    informational: bool = False
    informational_reason: str = ""


@dataclasses.dataclass(frozen=True)
class Catalogue:
    reference: dict
    runners: dict
    budgets: tuple

    def quick(self) -> tuple:
        return tuple(budget for budget in self.budgets if budget.quick)

    def ids(self) -> tuple:
        return tuple(budget.id for budget in self.budgets)

    def get(self, id_: str) -> Budget:
        for budget in self.budgets:
            if budget.id == id_:
                return budget
        raise BudgetError(f"unknown budget id {id_!r}")


def _number_text(value) -> str:
    """Format a maximum so a rewrite of the file reproduces it exactly."""
    if type(value) is int:
        return str(value)
    return repr(float(value))


def _positive(value, where):
    if isinstance(value, bool) or type(value) not in (int, float):
        raise BudgetError(f"{where}: expected a number, got {value!r}")
    if not math.isfinite(value):
        raise BudgetError(f"{where}: expected a finite number, got {value!r}")
    if value <= 0:
        raise BudgetError(f"{where}: must be greater than zero, got {value!r}")
    return value


def _string(value, where):
    if not isinstance(value, str) or not value.strip():
        raise BudgetError(f"{where}: expected a non-empty string, got {value!r}")
    return value.strip()


def _reference_blocks(text):
    """Split the raw text into one chunk per `[[budget]]` block."""
    blocks = []
    current = None
    for number, line in enumerate(text.splitlines(), start=1):
        if line.strip().startswith("[[budget]]"):
            current = []
            blocks.append((number, current))
        elif current is not None:
            current.append(line)
    return blocks


def _check_reference_comments(text, path, expected):
    """Every budget block must document the hardware it is for."""
    blocks = _reference_blocks(text)
    if len(blocks) != expected:
        raise BudgetError(
            f"{path}: {expected} budget entries but {len(blocks)} `[[budget]]` headers")
    for number, lines in blocks:
        for line in lines:
            match = REFERENCE_COMMENT.match(line.strip())
            if match and match.group(1).strip():
                break
        else:
            raise BudgetError(
                f"{path}:{number}: the `[[budget]]` block has no `# reference:` comment "
                "naming the reference hardware and bandwidth profile")


def _parse_runners(table, path):
    if not isinstance(table, dict):
        raise BudgetError(f"{path}: [runner] must be a table")
    runners = {}
    for label, entry in table.items():
        where = f"{path}: [runner.{label}]"
        if not RUNNER_LABEL.fullmatch(label):
            raise BudgetError(f"{where}: invalid runner label")
        if not isinstance(entry, dict) or set(entry) != {"multiplier"}:
            raise BudgetError(f"{where}: expected exactly one `multiplier` key")
        value = _positive(entry["multiplier"], f"{where}.multiplier")
        if value < 1:
            raise BudgetError(f"{where}: multipliers must be >= 1")
        if label == "reference" and value != 1:
            raise BudgetError(f"{where}: the reference multiplier must be 1")
        runners[label] = value
    return runners


def _parse_budget(entry, index, path, seen):
    where = f"{path}: budget #{index + 1}"
    if not isinstance(entry, dict):
        raise BudgetError(f"{where}: expected a table")
    unknown = set(entry) - set(REQUIRED_KEYS) - set(OPTIONAL_KEYS)
    missing = [key for key in REQUIRED_KEYS if key not in entry]
    if unknown:
        raise BudgetError(f"{where}: unknown key(s) {sorted(unknown)}")
    if missing:
        raise BudgetError(f"{where}: missing key(s) {missing}")
    id_ = entry["id"]
    if not isinstance(id_, str) or not BUDGET_ID.fullmatch(id_):
        raise BudgetError(f"{where}: invalid id {id_!r}")
    if id_ in seen:
        raise BudgetError(f"{where}: duplicate budget id {id_!r}")
    metric = entry["metric"]
    if metric not in METRICS:
        raise BudgetError(f"{where}: unknown metric {metric!r}, expected one of {METRICS}")
    unit = entry["unit"]
    if unit not in UNITS:
        raise BudgetError(f"{where}: unknown unit {unit!r}, expected one of {UNITS}")
    runner = entry["runner"]
    if runner not in RUNNERS:
        raise BudgetError(f"{where}: unknown runner {runner!r}, expected one of {RUNNERS}")
    profile = entry["profile"]
    if profile not in PROFILES:
        raise BudgetError(f"{where}: unknown profile {profile!r}, expected one of {PROFILES}")
    quick = entry["quick"]
    if not isinstance(quick, bool):
        raise BudgetError(f"{where}: `quick` must be true or false, got {quick!r}")
    proposed = entry.get("proposed", False)
    if not isinstance(proposed, bool):
        raise BudgetError(f"{where}: `proposed` must be true or false, got {proposed!r}")
    note = entry.get("note", "")
    if not isinstance(note, str):
        raise BudgetError(f"{where}: `note` must be a string")
    informational = entry.get("informational", False)
    if not isinstance(informational, bool):
        raise BudgetError(f"{where}: `informational` must be true or false, got {informational!r}")
    informational_reason = entry.get("informational_reason", "")
    if not isinstance(informational_reason, str):
        raise BudgetError(f"{where}: `informational_reason` must be a string")
    informational_reason = informational_reason.strip()
    if informational and not informational_reason:
        raise BudgetError(
            f"{where}: `informational = true` needs a non-empty `informational_reason` "
            "naming who decides and since when")
    if not informational and informational_reason:
        raise BudgetError(
            f"{where}: `informational_reason` is set but `informational` is not true")
    max_ = _positive(entry["max"], f"{where}.max")
    if metric in ("p50", "p95") and unit == "bytes":
        # A percentile of a byte count is legal but never what a budget means;
        # refusing it keeps the catalogue honest instead of odd.
        raise BudgetError(f"{where}: a {metric} of {unit} is not a meaningful budget")
    return Budget(id=id_, metric=metric, max=max_, unit=unit, quick=quick,
                  runner=runner, profile=profile, proposed=proposed,
                  note=note.strip(), informational=informational,
                  informational_reason=informational_reason)


def load(path) -> Catalogue:
    """Read and validate a `budgets.toml`; raise BudgetError on any problem."""
    path = Path(path)
    try:
        text = path.read_text(encoding="utf-8")
    except OSError as error:
        raise BudgetError(f"{path}: {error}") from error
    try:
        data = tomllib.loads(text)
    except ValueError as error:
        raise BudgetError(f"{path}: {error}") from error
    unknown = set(data) - {"reference", "runner", "budget"}
    if unknown:
        raise BudgetError(f"{path}: unknown top-level key(s) {sorted(unknown)}")
    reference = data.get("reference")
    if not isinstance(reference, dict):
        raise BudgetError(f"{path}: missing the [reference] table that states the hardware")
    unknown = set(reference) - set(REFERENCE_KEYS)
    if unknown:
        raise BudgetError(f"{path}: unknown [reference] key(s) {sorted(unknown)}")
    for key in ("hardware", "network"):
        if key not in reference:
            raise BudgetError(f"{path}: [reference] is missing `{key}`")
        _string(reference[key], f"{path}: [reference].{key}")
    if "browser" in reference:
        _string(reference["browser"], f"{path}: [reference].browser")
    runners = _parse_runners(data.get("runner", {}), path)
    entries = data.get("budget")
    if not isinstance(entries, list) or not entries:
        raise BudgetError(f"{path}: expected at least one [[budget]] entry")
    seen = set()
    budgets = []
    for index, entry in enumerate(entries):
        budget = _parse_budget(entry, index, path, seen)
        seen.add(budget.id)
        budgets.append(budget)
    _check_reference_comments(text, path, len(entries))
    return Catalogue(reference=reference, runners=runners, budgets=tuple(budgets))


def gate_text(catalogue: Catalogue) -> str:
    """Render the projection `tools/ci/ui/perf_gate.py` reads.

    Only `[[budget]]` entries with `GATE_KEYS` and the `[runner.<label>]`
    tables are emitted; the gate rejects any other key.
    """
    lines = [
        "# GENERATED FILE: do not edit.",
        "# Projection of tools/browser-budgets/budgets.toml into the format",
        "# tools/ci/ui/perf_gate.py reads. Regenerate with",
        "#   python3 tools/browser-budgets/budget_cli.py gate --write",
        "# A unit test fails when this file and budgets.toml disagree.",
        "",
    ]
    for label in sorted(catalogue.runners):
        lines.append(f"[runner.{label}]")
        lines.append(f"multiplier = {_number_text(catalogue.runners[label])}")
        lines.append("")
    for budget in catalogue.budgets:
        lines.append("[[budget]]")
        lines.append(f'id = "{budget.id}"')
        lines.append(f'metric = "{budget.metric}"')
        lines.append(f"max = {_number_text(budget.max)}")
        lines.append(f'unit = "{budget.unit}"')
        lines.append(f'runner = "{budget.runner}"')
        lines.append("")
    return "\n".join(lines)


def write_gate_catalogue(catalogue: Catalogue, path) -> bool:
    """Write the projection; return whether the file changed."""
    path = Path(path)
    text = gate_text(catalogue)
    try:
        current = path.read_text(encoding="utf-8")
    except OSError:
        current = None
    if current == text:
        return False
    path.write_text(text, encoding="utf-8")
    return True


def percentile(samples, p: float):
    """Nearest-rank percentile: always returns one of the samples, never a mean.

    Nearest rank keeps integers integral (a count of bytes or a whole
    millisecond comes back out as measured) and needs no interpolation.
    """
    if not samples:
        raise BudgetError("percentile of an empty sample set")
    ordered = sorted(samples)
    if not 0 < p <= 1:
        raise BudgetError(f"percentile out of range: {p!r}")
    rank = max(1, math.ceil(p * len(ordered)))
    return ordered[min(rank, len(ordered)) - 1]


def _check_samples(samples, where):
    values = []
    for sample in samples:
        if isinstance(sample, bool) or type(sample) not in (int, float):
            raise BudgetError(f"{where}: sample is not a number: {sample!r}")
        if not math.isfinite(sample) or sample < 0:
            raise BudgetError(f"{where}: sample must be finite and nonnegative: {sample!r}")
        values.append(sample)
    if not values:
        raise BudgetError(f"{where}: no samples")
    return values


def measure_value(samples, metric: str):
    """Reduce samples to the statistic the budget names.

    No floating-point arithmetic for counts: `max` and the percentiles select a
    measured value, so byte counts stay integers; `mean` uses integer division
    when every sample is an integer and only falls back to a float for timing
    samples (timings are not money or quantities).
    """
    values = _check_samples(samples, metric)
    if metric == "max":
        return max(values)
    if metric == "p50":
        return percentile(values, 0.5)
    if metric == "p95":
        return percentile(values, 0.95)
    if metric == "mean":
        total = sum(values)
        if all(type(value) is int for value in values):
            return total // len(values)
        return total / len(values)
    raise BudgetError(f"unknown metric {metric!r}")


@dataclasses.dataclass(frozen=True)
class Verdict:
    records: tuple
    missing: tuple
    breaches: tuple
    #: Breaches of an `informational = true` budget: reported, but never make
    #: `ok` false. A *missing* informational result is never in this tuple —
    #: it is always in `missing` (absence is never a pass, informational or not).
    informational_breaches: tuple = ()

    @property
    def ok(self) -> bool:
        return not self.missing and not self.breaches

    def reason(self) -> str:
        parts = []
        for id_, why in self.missing:
            parts.append(f"missing {id_}: {why}")
        for record in self.breaches:
            parts.append(f"breach {record['id']}: {record['measured']:g} > {record['max']:g} "
                         f"{record['unit']} ({record['metric']})")
        return "; ".join(parts)


def statistics(samples) -> dict:
    """The sample summary a reader needs to judge the number (p50/p95/max/mean)."""
    return {
        "count": len(samples),
        "p50": percentile(samples, 0.5),
        "p95": percentile(samples, 0.95),
        "max": max(samples),
        "mean": measure_value(samples, "mean"),
    }


def result_record(budget: Budget, measured, *, runner, commit, count, samples) -> dict:
    """Build one result record in the gate's format plus its own evidence.

    `status` is this runner's own verdict against the *unscaled* maximum; the
    gate re-derives the verdict from the budget files and may apply a reviewed
    multiplier, and it is the authority. The name `BREACH` (not `FAIL`) keeps
    the two vocabularies apart.
    """
    return {
        "id": budget.id,
        "measured": measured,
        "max": budget.max,
        "unit": budget.unit,
        "metric": budget.metric,
        "runner": runner,
        "commit": commit,
        "status": "PASS" if measured <= budget.max else "BREACH",
        "count": count,
        "stats": statistics(list(samples)),
        "samples": list(samples),
        "proposed": budget.proposed,
        "informational": budget.informational,
    }


def aggregate(catalogue: Catalogue, runs, *, runner, commit, extra=None) -> Verdict:
    """Reduce raw per-run samples into one record per budget plus a verdict.

    `runs` is a list of raw run objects (`{"run": n, "measures": {id: [..]},
    "errors": {id: message}}`). A budget with no sample is *missing*: absence
    is a failure, never a zero.
    """
    samples = {budget.id: [] for budget in catalogue.budgets}
    errors = {}
    for run in runs:
        measures = (run or {}).get("measures") or {}
        for id_, values in measures.items():
            if id_ in samples and isinstance(values, list):
                samples[id_].extend(values)
        for id_, message in ((run or {}).get("errors") or {}).items():
            errors.setdefault(id_, str(message))
    records, missing, breaches = [], [], []
    for budget in catalogue.budgets:
        values = samples[budget.id]
        if not values:
            missing.append((budget.id, errors.get(budget.id, "the driver produced no sample")))
            continue
        measured = measure_value(values, budget.metric)
        record = result_record(budget, measured, runner=runner, commit=commit,
                              count=len(values), samples=values)
        if extra:
            for key, value in extra.items():
                record.setdefault(key, value)
        records.append(record)
        if measured > budget.max:
            breaches.append(record)
    informational_breaches = [record for record in breaches
                              if catalogue.get(record["id"]).informational]
    breaches = [record for record in breaches
                if not catalogue.get(record["id"]).informational]
    return Verdict(tuple(records), tuple(missing), tuple(breaches),
                   tuple(informational_breaches))


def no_duplicates(pairs):
    result = {}
    for key, value in pairs:
        if key in result:
            raise BudgetError(f"duplicate JSON key {key}")
        result[key] = value
    return result


def write_results(directory: Path, verdict: Verdict) -> list:
    """Write `<id>.json` per record; only these files may live in the directory."""
    directory = Path(directory)
    directory.mkdir(parents=True, exist_ok=True)
    written = []
    for record in verdict.records:
        path = directory / f"{record['id']}.json"
        path.write_text(json.dumps(record, indent=2, sort_keys=True) + "\n", encoding="utf-8")
        written.append(path)
    return written


def read_runs(directory) -> list:
    """Read `run-*.json` raw files in run order."""
    directory = Path(directory)
    if not directory.is_dir():
        raise BudgetError(f"raw directory missing: {directory}")
    runs = []
    for path in sorted(directory.glob("run-*.json")):
        try:
            runs.append(json.loads(path.read_text(encoding="utf-8"), object_pairs_hook=no_duplicates))
        except (OSError, ValueError) as error:
            raise BudgetError(f"{path}: {error}") from error
    if not runs:
        raise BudgetError(f"no run-*.json raw files in {directory}")
    return runs


def check_results(directory, catalogue: Catalogue, *, runner, commit) -> list:
    """Validate a result directory; return a list of problems (empty is good).

    Checks the files the gate will read: one result per budget, no unknown or
    extra id, the required schema, `max`/`metric`/`unit`/`runner`/`commit`
    equal to the declaration, and no breach of the unscaled maximum.
    """
    directory = Path(directory)
    problems = []
    if not directory.is_dir():
        return [f"result directory missing: {directory}"]
    by_id = {budget.id: budget for budget in catalogue.budgets}
    seen = set()
    for path in sorted(directory.rglob("*.json")):
        try:
            record = json.loads(path.read_text(encoding="utf-8"), object_pairs_hook=no_duplicates)
        except (OSError, ValueError) as error:
            problems.append(f"{path}: {error}")
            continue
        if not isinstance(record, dict) or set(RESULT_REQUIRED) - set(record):
            problems.append(f"{path}: missing result fields")
            continue
        id_ = record["id"]
        if id_ not in by_id:
            problems.append(f"{path}: unknown budget id {id_!r}")
            continue
        if id_ in seen:
            problems.append(f"{path}: second result for {id_!r}")
            continue
        seen.add(id_)
        budget = by_id[id_]
        for key, expected in (("max", budget.max), ("metric", budget.metric),
                              ("unit", budget.unit), ("runner", runner),
                              ("commit", commit)):
            if record[key] != expected:
                problems.append(f"{path}: {key} is {record[key]!r}, expected {expected!r}")
        if record.get("informational", False) != budget.informational:
            problems.append(f"{path}: informational is {record.get('informational')!r}, "
                           f"expected {budget.informational!r}")
        measured = record["measured"]
        if isinstance(measured, bool) or type(measured) not in (int, float):
            problems.append(f"{path}: measured is not a number: {measured!r}")
        elif not math.isfinite(measured) or measured < 0:
            problems.append(f"{path}: measured must be finite and nonnegative")
        elif measured > budget.max and not budget.informational:
            # An informational budget's breach is reported (the record still says
            # BREACH) but never fails the check: absence is the only thing that
            # always fails for it.
            problems.append(f"{path}: breach, {measured:g} > {budget.max:g} {budget.unit}")
    for id_ in by_id:
        if id_ not in seen:
            problems.append(f"{directory}: no result for budget {id_!r}")
    return problems
