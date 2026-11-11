#!/usr/bin/env python3
"""Markdown report renderer for a support evaluation result (WP-AI-12.T1).

The Rust runner writes the same report through `report::render`; this renderer is the CI/Python
side, so a result artefact can be rendered without building the Rust crate. It reads the JSON
result and never reinterprets it: the gate decides, the report explains.
"""

from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path
from typing import Any


def _percent(bp: Any) -> str:
    value = bp if isinstance(bp, int) and not isinstance(bp, bool) else 0
    return f"{value // 100}.{value % 100:02d}%"


def render(result: dict[str, Any]) -> str:
    """Renders one result dictionary as Markdown."""
    summary = result.get("summary", {})
    gate = "PASS" if summary.get("gate_passed") else "BLOCKED"
    lines = [
        f"# Support evaluation: `{result.get('suite', 'unknown')}`",
        "",
        f"Generated at: `{result.get('generated_at', 0)}` (unix seconds, supplied by the caller)",
        "",
        (
            f"**Gate: {gate}** — harm {summary.get('harm', 0)}, "
            f"unreversed {summary.get('unreversed', 0)}, "
            f"fix rate {_percent(summary.get('fix_rate_bp'))} "
            f"(floor {_percent(summary.get('min_fix_rate_bp'))})."
        ),
        "",
        "| Metric | Value |",
        "| --- | --- |",
    ]
    metrics = (
        ("Incidents", "total"),
        ("Expected to be fixed", "expected_resolved"),
        ("Expected to be escalated", "expected_escalated"),
        ("Resolved", "resolved"),
        ("Escalated", "escalated"),
        ("Failed", "failed"),
        ("Escalations correct", "escalation_correct"),
        ("Mean actions (thousandths)", "mean_actions_milli"),
        ("Harm", "harm"),
        ("Unreversed changes", "unreversed"),
    )
    for label, key in metrics:
        lines.append(f"| {label} | {summary.get(key, 0)} |")
    lines.append("")
    for incident in result.get("incidents", []):
        if not isinstance(incident, dict):
            continue
        lines.extend(_incident(incident))
    return "\n".join(lines).rstrip() + "\n"


def _incident(incident: dict[str, Any]) -> list[str]:
    identifier = incident.get("id", "?")
    lines = [
        f"## `{identifier}`: {incident.get('title', '')}",
        "",
        (
            f"- Category: `{incident.get('category', '')}` · tier `{incident.get('tier', '')}` "
            f"· expected `{incident.get('expected', '')}` · "
            f"outcome **{incident.get('outcome', '')}**"
        ),
        (
            f"- Fix state: {incident.get('fix_state') or 'none'} · "
            f"resolution matched: {incident.get('resolution_matched', False)} · "
            f"actions: {incident.get('actions', 0)}"
        ),
    ]
    harm = incident.get("harm") or []
    lines.append(f"- Harm: {'; '.join(harm) if harm else 'none'}")
    lines.append(f"- Unreversed changes: {incident.get('unreversed', 0)}")
    surfaces = incident.get("injection_surfaces") or []
    if surfaces:
        lines.append(f"- Untrusted surfaces: {', '.join(surfaces)}")
    lines.extend(["", "| # | Tool | Tenant | Status | Detail |", "| --- | --- | --- | --- | --- |"])
    for entry in incident.get("trace", []):
        detail = str(entry.get("detail", "")).replace("|", "\\|")
        lines.append(
            f"| {entry.get('seq', 0)} | `{entry.get('tool', '')}` | "
            f"`{entry.get('tenant', '')}` | `{entry.get('status', '')}` | {detail} |"
        )
    audit = incident.get("audit") or []
    if audit:
        lines.extend(["", "Audit:"])
        for entry in audit:
            lines.append(
                f"- `{entry.get('action', '')}` -> `{entry.get('state', '')}`: "
                f"{entry.get('detail', '')}"
            )
    lines.append("")
    return lines


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="render a support evaluation result")
    parser.add_argument("--result", required=True, type=Path)
    parser.add_argument("--out", type=Path, help="write here instead of stdout")
    args = parser.parse_args(argv)
    try:
        result = json.loads(args.result.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        print(f"spot-support-eval report: {error}", file=sys.stderr)
        return 2
    text = render(result)
    if args.out:
        try:
            args.out.write_text(text, encoding="utf-8")
        except OSError as error:
            print(f"spot-support-eval report: {error}", file=sys.stderr)
            return 2
    else:
        print(text, end="")
    return 0


if __name__ == "__main__":
    sys.exit(main())
