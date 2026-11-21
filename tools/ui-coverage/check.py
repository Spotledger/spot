#!/usr/bin/env python3
"""WP-UI-06.T1: screen coverage tracker.

Compares the screens manifest (what approved designs require) with the screens
the Avalonia code implements ([ScreenId("...")]) and the UI tests that cover them
([CoversScreen("...", "state", ...)]). Python standard library only; the scan is
a static read of source text, so no build is needed. A reflection exporter in the
.NET analytics tests may also write screens-found.json, merged with --found.

Exit codes: 0 ok, 1 thresholds or regressions failed, 2 bad input.
"""
from __future__ import annotations

import argparse
import json
import re
import sys
import tomllib
from pathlib import Path

KINDS = ("form", "list", "report", "dashboard", "dialog", "workspace")
PRIORITIES = ("p0", "p1", "p2")
TOKEN = re.compile(r"^[a-z0-9][a-z0-9._-]{0,63}$")
STATE_TOKEN = re.compile(r"^[a-z][a-z0-9_-]{0,31}$")
DEFAULT_EXCLUDE_DIRS = ("bin", "obj", ".git", "node_modules", "Spotledger.Client.Analytics.Tests")

_COMMENTS = re.compile(
    r'"(?:\\.|[^"\\\n])*"|\'(?:\\.|[^\'\\\n])*\'|//[^\n]*|/\*.*?\*/', re.S
)
_SCREEN_ID = re.compile(r'\[\s*(?:[\w.]+\s*:\s*)?(?:[\w.]*\.)?ScreenId(?:Attribute)?\s*\(\s*"([^"\n]*)"')
_COVERS = re.compile(
    r'\[\s*(?:[\w.]+\s*:\s*)?(?:[\w.]*\.)?CoversScreen(?:Attribute)?\s*\(\s*"([^"\n]*)"((?:\s*,\s*"[^"\n]*")*)\s*\)'
)
_STR = re.compile(r'"([^"\n]*)"')


class InputError(Exception):
    pass


def strip_comments(text: str) -> str:
    """Remove comments but keep string literals (attribute arguments live in them)."""

    def repl(m: re.Match) -> str:
        s = m.group(0)
        if s[0] in "\"'":
            return s
        return "" if s.startswith("//") else "\n" * s.count("\n")

    return _COMMENTS.sub(repl, text)


def load_manifest(directory: Path, include_examples: bool = False) -> dict[str, dict]:
    if not directory.is_dir():
        raise InputError(f"screens directory not found: {directory}")
    screens: dict[str, dict] = {}
    for path in sorted(directory.glob("*.toml")):
        if path.name.startswith("example") and not include_examples:
            continue
        try:
            data = tomllib.loads(path.read_text(encoding="utf-8"))
        except (tomllib.TOMLDecodeError, UnicodeDecodeError) as e:
            raise InputError(f"{path.name}: invalid TOML: {e}") from e
        unknown = set(data) - {"screen"}
        if unknown:
            raise InputError(f"{path.name}: unknown top-level keys {sorted(unknown)}")
        for i, raw in enumerate(data.get("screen", []), 1):
            where = f"{path.name} screen #{i}"
            s = validate_screen(raw, where)
            if s["id"] in screens:
                raise InputError(f"{where}: duplicate screen id {s['id']!r}")
            screens[s["id"]] = s
    return screens


def validate_screen(raw: dict, where: str) -> dict:
    allowed = {"id", "title", "module", "kind", "doctype", "route", "design_ref", "required_states", "priority"}
    if not isinstance(raw, dict):
        raise InputError(f"{where}: not a table")
    extra = set(raw) - allowed
    if extra:
        raise InputError(f"{where}: unknown keys {sorted(extra)}")
    for key in ("id", "title", "module", "kind", "design_ref", "priority"):
        if not isinstance(raw.get(key), str) or not raw[key].strip():
            raise InputError(f"{where}: {key} is required and must be a non-empty string")
    if not TOKEN.match(raw["id"]):
        raise InputError(f"{where}: id {raw['id']!r} must match {TOKEN.pattern}")
    if not TOKEN.match(raw["module"]):
        raise InputError(f"{where}: module {raw['module']!r} must match {TOKEN.pattern}")
    if raw["kind"] not in KINDS:
        raise InputError(f"{where}: kind must be one of {', '.join(KINDS)}")
    if raw["priority"] not in PRIORITIES:
        raise InputError(f"{where}: priority must be one of {', '.join(PRIORITIES)}")
    for key in ("doctype", "route"):
        if key in raw and not isinstance(raw[key], str):
            raise InputError(f"{where}: {key} must be a string")
    states = raw.get("required_states", [])
    if not isinstance(states, list) or not all(isinstance(x, str) and STATE_TOKEN.match(x) for x in states):
        raise InputError(f"{where}: required_states must be a list of lowercase state names")
    if len(set(states)) != len(states):
        raise InputError(f"{where}: required_states has duplicates")
    s = dict(raw)
    s["required_states"] = list(states)
    return s


def _source_files(root: Path, exclude_dirs: tuple[str, ...]):
    if not root.exists():
        return
    for p in sorted(root.rglob("*.cs")):
        if any(part in exclude_dirs for part in p.relative_to(root).parts[:-1]):
            continue
        yield p


def scan_implemented(root: Path, exclude_dirs=DEFAULT_EXCLUDE_DIRS) -> dict[str, list[str]]:
    """screen id -> source files (relative to root) that declare it."""
    found: dict[str, list[str]] = {}
    for p in _source_files(root, exclude_dirs):
        text = strip_comments(p.read_text(encoding="utf-8-sig", errors="replace"))
        for m in _SCREEN_ID.finditer(text):
            found.setdefault(m.group(1), []).append(p.relative_to(root).as_posix())
    return found


def scan_covered(root: Path, exclude_dirs=DEFAULT_EXCLUDE_DIRS) -> dict[str, dict]:
    """screen id -> {"states": set, "files": [..]}. A CoversScreen with no states covers 'default'."""
    found: dict[str, dict] = {}
    for p in _source_files(root, exclude_dirs):
        text = strip_comments(p.read_text(encoding="utf-8-sig", errors="replace"))
        for m in _COVERS.finditer(text):
            entry = found.setdefault(m.group(1), {"states": set(), "files": []})
            states = _STR.findall(m.group(2) or "") or ["default"]
            entry["states"].update(states)
            rel = p.relative_to(root).as_posix()
            if rel not in entry["files"]:
                entry["files"].append(rel)
    return found


def merge_found(implemented: dict, covered: dict, path: Path) -> None:
    """Merge the reflection exporter's screens-found.json into the static scan."""
    try:
        data = json.loads(path.read_text(encoding="utf-8"))
        impl = data.get("implemented", {})
        cov = data.get("covered", {})
        if not isinstance(impl, dict) or not isinstance(cov, dict):
            raise ValueError("implemented and covered must be objects")
    except (OSError, ValueError) as e:
        raise InputError(f"{path}: cannot read screens-found.json: {e}") from e
    for sid, where in impl.items():
        lst = implemented.setdefault(sid, [])
        lst.append(str(where) if not isinstance(where, list) else ",".join(map(str, where)))
    for sid, info in cov.items():
        states = info.get("states", []) if isinstance(info, dict) else []
        entry = covered.setdefault(sid, {"states": set(), "files": []})
        entry["states"].update(states or ["default"])
        entry["files"].append("reflection")


def evaluate(manifest: dict[str, dict], implemented: dict, covered: dict) -> dict:
    rows = []
    for sid, s in sorted(manifest.items()):
        is_impl = sid in implemented
        cov = covered.get(sid)
        has_test = cov is not None
        missing_states = sorted(set(s["required_states"]) - (cov["states"] if cov else set()))
        # a required-state check only makes sense once the screen is tested; "default" is implicit
        rows.append(
            {
                "id": sid, "title": s["title"], "module": s["module"], "kind": s["kind"],
                "priority": s["priority"], "design_ref": s["design_ref"],
                "implemented": is_impl, "tested": has_test,
                "states_complete": has_test and not missing_states,
                "missing_states": missing_states,
            }
        )

    def tally(group):
        t = {"declared": 0, "implemented": 0, "covered": 0, "covered_for_states": 0}
        for r in group:
            t["declared"] += 1
            t["implemented"] += r["implemented"]
            t["covered"] += r["tested"]
            t["covered_for_states"] += r["states_complete"]
        return t

    by_module, by_kind = {}, {}
    for r in rows:
        by_module.setdefault(r["module"], []).append(r)
        by_kind.setdefault(r["kind"], []).append(r)
    return {
        "totals": tally(rows),
        "by_module": {k: tally(v) for k, v in sorted(by_module.items())},
        "by_kind": {k: tally(v) for k, v in sorted(by_kind.items())},
        "screens": rows,
        "missing": [r for r in rows if not (r["implemented"] and r["states_complete"])],
        "unknown_implemented": sorted(set(implemented) - set(manifest)),
        "unknown_covered": sorted(set(covered) - set(manifest)),
    }


def gaps(report: dict) -> dict:
    """The gap set a baseline records: ids not implemented, not tested, or missing states."""
    return {
        "unimplemented": sorted(r["id"] for r in report["screens"] if not r["implemented"]),
        "untested": sorted(r["id"] for r in report["screens"] if not r["tested"]),
        "missing_states": {
            r["id"]: r["missing_states"] for r in report["screens"] if r["tested"] and r["missing_states"]
        },
    }


def regressions(report: dict, baseline: dict) -> list[str]:
    cur = gaps(report)
    out = []
    for sid in cur["unimplemented"]:
        if sid not in baseline.get("unimplemented", []):
            out.append(f"{sid}: not implemented (new gap)")
    for sid in cur["untested"]:
        if sid not in baseline.get("untested", []):
            out.append(f"{sid}: no UI test covers it (new gap)")
    for sid, states in cur["missing_states"].items():
        if sid in baseline.get("untested", []):
            continue  # already a known gap: untested screens had no states covered
        old = set(baseline.get("missing_states", {}).get(sid, []))
        new = sorted(set(states) - old)
        if new:
            out.append(f"{sid}: required states not covered: {', '.join(new)} (new gap)")
    return out


def load_baseline(path: Path) -> dict:
    try:
        data = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, ValueError) as e:
        raise InputError(f"{path}: cannot read baseline: {e}") from e
    if not isinstance(data, dict):
        raise InputError(f"{path}: baseline must be a JSON object")
    return data


def pct(n: int, d: int) -> float:
    return 100.0 if d == 0 else round(100.0 * n / d, 1)


def to_markdown(report: dict, problems: list[str]) -> str:
    t = report["totals"]
    lines = [
        "# UI screen coverage", "",
        f"Declared {t['declared']}, implemented {t['implemented']} ({pct(t['implemented'], t['declared'])}%), "
        f"covered by a test {t['covered']} ({pct(t['covered'], t['declared'])}%), "
        f"covered for required states {t['covered_for_states']}.", "",
    ]
    for title, key in (("By module", "by_module"), ("By kind", "by_kind")):
        lines += [f"## {title}", "", "| Name | Declared | Implemented | Tested | All required states |", "| --- | ---: | ---: | ---: | ---: |"]
        for name, v in report[key].items():
            lines.append(f"| {name} | {v['declared']} | {v['implemented']} | {v['covered']} | {v['covered_for_states']} |")
        lines.append("")
    lines += ["## Missing", ""]
    if report["missing"]:
        lines += ["| Screen | Module | Kind | Priority | Implemented | Tested | Missing states | Design |", "| --- | --- | --- | --- | --- | --- | --- | --- |"]
        for r in report["missing"]:
            lines.append(
                f"| {r['id']} | {r['module']} | {r['kind']} | {r['priority']} | {'yes' if r['implemented'] else 'no'} | "
                f"{'yes' if r['tested'] else 'no'} | {', '.join(r['missing_states']) or '-'} | {r['design_ref']} |"
            )
    else:
        lines.append("None.")
    lines.append("")
    for title, key in (("Implemented but not in the manifest", "unknown_implemented"), ("Tests naming a screen not in the manifest", "unknown_covered")):
        if report[key]:
            lines += [f"## {title}", ""] + [f"- {x}" for x in report[key]] + [""]
    if problems:
        lines += ["## Failures", ""] + [f"- {p}" for p in problems] + [""]
    return "\n".join(lines)


def main(argv=None) -> int:
    here = Path(__file__).resolve().parent
    repo = here.parent.parent
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--screens", type=Path, default=here / "screens", help="directory of manifest *.toml files")
    ap.add_argument("--src", type=Path, default=repo / "clients/avalonia/src", help="root scanned for [ScreenId]")
    ap.add_argument("--tests", type=Path, default=repo / "clients/avalonia/tests", help="root scanned for [CoversScreen]")
    ap.add_argument("--found", type=Path, help="screens-found.json written by the reflection exporter")
    ap.add_argument("--baseline", type=Path, help="baseline.json (default: <screens>/baseline.json when --regressions-only)")
    ap.add_argument("--regressions-only", action="store_true", help="fail only on gaps absent from the baseline")
    ap.add_argument("--write-baseline", action="store_true", help="record the current gaps as the baseline")
    ap.add_argument("--min-implemented", type=float, default=None, help="minimum implemented percent")
    ap.add_argument("--min-covered", type=float, default=None, help="minimum tested percent")
    ap.add_argument("--json", type=Path, help="write the JSON report here")
    ap.add_argument("--markdown", type=Path, help="write the Markdown report here")
    ap.add_argument("--include-examples", action="store_true")
    a = ap.parse_args(argv)
    try:
        manifest = load_manifest(a.screens, a.include_examples)
        implemented = scan_implemented(a.src)
        covered = scan_covered(a.tests)
        if a.found:
            merge_found(implemented, covered, a.found)
        report = evaluate(manifest, implemented, covered)
        baseline_path = a.baseline or a.screens / "baseline.json"
        problems: list[str] = []
        if a.write_baseline:
            baseline_path.write_text(json.dumps(gaps(report), indent=2, sort_keys=True) + "\n", encoding="utf-8")
        t = report["totals"]
        if a.min_implemented is not None and pct(t["implemented"], t["declared"]) < a.min_implemented:
            problems.append(f"implemented {pct(t['implemented'], t['declared'])}% is below --min-implemented {a.min_implemented}%")
        if a.min_covered is not None and pct(t["covered"], t["declared"]) < a.min_covered:
            problems.append(f"covered {pct(t['covered'], t['declared'])}% is below --min-covered {a.min_covered}%")
        if a.regressions_only:
            problems += regressions(report, load_baseline(baseline_path))
        problems += [f"{x}: test names a screen that is not in the manifest" for x in report["unknown_covered"]]
    except InputError as e:
        print(f"error: {e}", file=sys.stderr)
        return 2
    report["problems"] = problems
    if a.json:
        a.json.write_text(json.dumps(report, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    md = to_markdown(report, problems)
    if a.markdown:
        a.markdown.write_text(md + "\n", encoding="utf-8")
    print(md)
    return 1 if problems else 0


if __name__ == "__main__":
    sys.exit(main())
