#!/usr/bin/env python3
"""L9 trend file and regression flags (WP-TST-15.T8).

`l9-trend.jsonl` has one line per run of an L9 profile: commit, runner, run id, exit status and
each budget's measured value. `append` reads a run's output (an l9-scale `report.json`, or the
captured test output, whose Markdown report carries the budget table and, for the soak, the leak
table), `show` prints the last 14 runs of each profile and flags a budget that worsened 15 percent
over the median of the seven runs before it even when it is still within budget, and `issue`
writes the body of the `L9 regression` issue from the newest runs.

Only latencies, counts and limits are kept: the fixed fields of a budget line (operation, metric,
limit, measured, verdict) and nothing else, so a report that carried a query text or a header
value could not leak it into the trend file. All arithmetic is integer: values are kept in
thousandths (milliseconds, operations per second) or parts per million.

    trend.py append --trend FILE --profile P --runner R --commit SHA --run ID --report PATH [--status N]
    trend.py show   --trend FILE [--profile P] [--last 14]
    trend.py issue  --trend FILE
"""
from __future__ import annotations

import argparse
import json
import re
import statistics
import sys
from pathlib import Path
from typing import Any, Iterable, Sequence

LAST = 14
MEDIAN_OF = 7
WORSENED_PERCENT = 15
TITLE = "L9 regression"
# Metrics where a smaller number is worse; every other metric is worse when it grows.
HIGHER_IS_BETTER = {"throughput"}
BUDGET_ROW = re.compile(
    r"^\|\s*(?P<op>[a-z_]+|all)\s*\|\s*(?P<metric>p95_ms|p99_ms|error_rate|throughput)\s*\|"
    r"\s*(?P<limit>[^|]+?)\s*\|\s*(?P<measured>[^|]+?)\s*\|\s*(?P<result>pass|FAIL)\s*\|\s*$"
)
LEAK_ROW = re.compile(
    r"^\|\s*(?P<series>[a-z_]+)\s*\|\s*(?P<check>[^|]+?)\s*\|\s*(?P<limit>[^|]+?)\s*\|"
    r"\s*(?P<measured>[^|]+?)\s*\|\s*(?P<result>pass|FAIL|not measured)\s*\|\s*[^|]*\|\s*$"
)
NUMBER = re.compile(r"^-?(\d+)(?:\.(\d+))?")


def to_milli(text: str) -> int | None:
    """The leading number of `12.345 ms`, `9.500 ops/s`, `0.0100 %` or `2047 kB per 10 min`,
    as thousandths (a percent as parts per million); None for `no samples` and the like."""
    m = NUMBER.match(text.strip())
    if not m:
        return None
    whole, frac = m.group(1), m.group(2) or ""
    negative = text.strip().startswith("-")
    value = int(whole) * 1000 + int((frac + "000")[:3])
    if text.strip().endswith("%"):
        # `1.2345 %` is 12 345 ppm: whole percent x 10 000 plus four fraction digits.
        value = int(whole) * 10_000 + int((frac + "0000")[:4])
    return -value if negative else value


def budgets_from_markdown(text: str) -> list[dict[str, Any]]:
    """The budget lines (and soak leak lines) of a Markdown report or a captured log."""
    out: list[dict[str, Any]] = []
    for raw in text.splitlines():
        line = raw.strip()
        m = BUDGET_ROW.match(line)
        if m:
            out.append(
                {
                    "operation": m["op"],
                    "metric": m["metric"],
                    "measured": to_milli(m["measured"]),
                    "limit": to_milli(m["limit"]),
                    "passed": m["result"] == "pass",
                }
            )
            continue
        m = LEAK_ROW.match(line)
        if m and m["series"] not in {"operation"} and m["result"] != "not measured":
            out.append(
                {
                    "operation": m["series"],
                    "metric": re.sub(r"\W+", "_", m["check"]).strip("_"),
                    "measured": to_milli(m["measured"]),
                    "limit": to_milli(m["limit"]),
                    "passed": m["result"] == "pass",
                }
            )
    return out


def budgets_from_json(text: str) -> list[dict[str, Any]]:
    """The budget lines of an l9-scale `report.json` (its `budgets` list; nothing else is read)."""
    data = json.loads(text)
    out = []
    for v in data.get("budgets", []) if isinstance(data, dict) else []:
        if not isinstance(v, dict):
            continue
        out.append(
            {
                "operation": str(v.get("operation", "")),
                "metric": str(v.get("metric", "")),
                "measured": to_milli(str(v.get("measured", ""))),
                "limit": to_milli(str(v.get("limit", ""))),
                "passed": bool(v.get("passed", False)),
            }
        )
    return out


def read_report(path: Path) -> list[dict[str, Any]]:
    """Budget lines from a report file: JSON when it parses as the report, Markdown otherwise."""
    text = path.read_text(encoding="utf-8", errors="replace")
    if text.lstrip().startswith("{"):
        try:
            return budgets_from_json(text)
        except ValueError:
            pass
    return budgets_from_markdown(text)


def make_record(profile: str, runner: str, commit: str, run: str, status: int, budgets: list[dict[str, Any]]) -> dict[str, Any]:
    return {
        "run": run,
        "commit": commit,
        "runner": runner,
        "profile": profile,
        "status": status,
        "budgets": budgets,
    }


def load(trend: Path) -> list[dict[str, Any]]:
    if not trend.is_file():
        return []
    records = []
    for line in trend.read_text(encoding="utf-8").splitlines():
        if line.strip():
            records.append(json.loads(line))
    return records


def key(b: dict[str, Any]) -> str:
    return f"{b['operation']}/{b['metric']}"


def worsened(history: Sequence[int], latest: int, metric: str) -> bool:
    """True when `latest` is at least 15 percent worse than the median of the last 7 of
    `history` (needs at least 3 earlier runs; integer arithmetic)."""
    window = [h for h in history[-MEDIAN_OF:] if h is not None]
    if len(window) < 3:
        return False
    median = int(statistics.median(window))
    if median <= 0:
        return False
    if metric.split("/")[-1] in HIGHER_IS_BETTER:
        return latest * 100 < median * (100 - WORSENED_PERCENT)
    return latest * 100 > median * (100 + WORSENED_PERCENT)


def flags(records: Iterable[dict[str, Any]]) -> list[dict[str, Any]]:
    """Per profile, the budgets of the newest run that worsened 15 percent over the median of
    the seven runs before it, whether or not they still pass."""
    by_profile: dict[str, list[dict[str, Any]]] = {}
    for r in records:
        by_profile.setdefault(r["profile"], []).append(r)
    found = []
    for profile, runs in sorted(by_profile.items()):
        if len(runs) < 2:
            continue
        newest, earlier = runs[-1], runs[:-1]
        for b in newest["budgets"]:
            if b["measured"] is None:
                continue
            history = []
            for r in earlier:
                for e in r["budgets"]:
                    if key(e) == key(b) and e["measured"] is not None:
                        history.append(e["measured"])
            if worsened(history, b["measured"], key(b)):
                found.append({"profile": profile, "budget": key(b), "measured": b["measured"], "run": newest["run"]})
    return found


def failing(records: Iterable[dict[str, Any]]) -> list[dict[str, Any]]:
    """The failing budgets of the newest run of each profile."""
    newest: dict[str, dict[str, Any]] = {}
    for r in records:
        newest[r["profile"]] = r
    out = []
    for profile, r in sorted(newest.items()):
        for b in r["budgets"]:
            if not b["passed"]:
                out.append({"profile": profile, "budget": key(b), "measured": b["measured"], "limit": b["limit"], "run": r["run"]})
        if r["status"] != 0 and not any(not b["passed"] for b in r["budgets"]):
            out.append({"profile": profile, "budget": "exit status", "measured": r["status"], "limit": 0, "run": r["run"]})
    return out


def show(records: list[dict[str, Any]], profile: str | None, last: int) -> str:
    lines = []
    marked = {(f["profile"], f["budget"]) for f in flags(records)}
    for name in sorted({r["profile"] for r in records}):
        if profile and name != profile:
            continue
        runs = [r for r in records if r["profile"] == name][-last:]
        lines.append(f"== {name} (last {len(runs)} runs)")
        budgets = sorted({key(b) for r in runs for b in r["budgets"]})
        for k in budgets:
            values = []
            for r in runs:
                v = next((b["measured"] for b in r["budgets"] if key(b) == k), None)
                values.append("-" if v is None else str(v))
            flag = "  WORSENED" if (name, k) in marked else ""
            lines.append(f"{k:<32} {' '.join(values)}{flag}")
        lines.append("")
    return "\n".join(lines)


def issue_body(records: list[dict[str, Any]]) -> str:
    bad = failing(records)
    slow = flags(records)
    lines = [f"# {TITLE}", ""]
    lines.append("Failing budgets of the newest run of each L9 profile (values in thousandths, percent budgets in ppm):")
    lines.append("")
    if bad:
        lines += ["| Profile | Budget | Measured | Limit | Run |", "| --- | --- | ---: | ---: | --- |"]
        lines += [f"| {f['profile']} | {f['budget']} | {f['measured']} | {f['limit']} | {f['run']} |" for f in bad]
    else:
        lines.append("None.")
    lines += ["", "Worsened 15 percent or more over the median of the previous 7 runs (still within budget or not):", ""]
    if slow:
        lines += ["| Profile | Budget | Measured | Run |", "| --- | --- | ---: | --- |"]
        lines += [f"| {f['profile']} | {f['budget']} | {f['measured']} | {f['run']} |" for f in slow]
    else:
        lines.append("None.")
    return "\n".join(lines) + "\n"


def main(argv: Sequence[str] | None = None) -> int:
    p = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    sub = p.add_subparsers(dest="command", required=True)
    a = sub.add_parser("append")
    a.add_argument("--trend", required=True)
    a.add_argument("--profile", required=True)
    a.add_argument("--runner", required=True)
    a.add_argument("--commit", required=True)
    a.add_argument("--run", required=True)
    a.add_argument("--status", type=int, default=0)
    a.add_argument("--report", action="append", default=[])
    s = sub.add_parser("show")
    s.add_argument("--trend", required=True)
    s.add_argument("--profile")
    s.add_argument("--last", type=int, default=LAST)
    i = sub.add_parser("issue")
    i.add_argument("--trend", required=True)
    args = p.parse_args(sys.argv[1:] if argv is None else argv)
    trend = Path(args.trend)
    if args.command == "append":
        budgets: list[dict[str, Any]] = []
        for report in args.report:
            path = Path(report)
            if path.is_file():
                budgets.extend(read_report(path))
        record = make_record(args.profile, args.runner, args.commit, args.run, args.status, budgets)
        trend.parent.mkdir(parents=True, exist_ok=True)
        with trend.open("a", encoding="utf-8") as f:
            f.write(json.dumps(record, sort_keys=True) + "\n")
        return 0
    records = load(trend)
    if args.command == "show":
        print(show(records, args.profile, args.last))
        return 0
    print(issue_body(records), end="")
    return 1 if failing(records) else 0


if __name__ == "__main__":
    sys.exit(main())
