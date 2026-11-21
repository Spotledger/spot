"""Shared helpers for the browser budget tests.

Puts `tools/browser-budgets` on `sys.path` so `import budgets` works both under
`python -m unittest discover -s tools/browser-budgets/tests` and when a single
test file is run directly. Tests use no network and no browser.
"""

from __future__ import annotations

import json
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
if str(ROOT) not in sys.path:
    sys.path.insert(0, str(ROOT))

import budgets  # noqa: E402  (import after the path fix on purpose)

BUDGETS = ROOT / "budgets.toml"
GATE = ROOT / "perf-budgets.toml"
BUDGET_IDS = (
    "browser.download.compressed",
    "browser.startup.cold",
    "browser.startup.warm",
    "browser.form.open",
    "browser.list.scroll.frame",
    "browser.child-edit.key",
    "browser.tabs.memory",
)

ENTRY = {
    "id": "test.measure",
    "metric": "p95",
    "max": 100,
    "unit": "ms",
    "quick": True,
    "runner": "reference",
    "profile": "local",
    "comment": "# reference: test hardware, 50 Mbit/s, no throttle",
}


def render(entries, *, reference=True, runners=True) -> str:
    """Render a catalogue from entry dicts; a falsy `comment` omits the line."""
    lines = []
    if reference:
        lines += ['[reference]',
                  'hardware = "test hardware: 4 cores, GPU"',
                  'network = "test network: 50 Mbit/s, 20 ms"',
                  '']
    if runners:
        lines += ['[runner.reference]', 'multiplier = 1.0', '']
    for entry in entries:
        settings = dict(ENTRY)
        settings.update(entry)
        lines.append("[[budget]]")
        if settings.get("comment"):
            lines.append(settings["comment"])
        for key in ("id", "metric", "max", "unit"):
            value = settings[key]
            if isinstance(value, bool):
                lines.append(f"{key} = {'true' if value else 'false'}")
            elif isinstance(value, str):
                lines.append(f'{key} = "{value}"')
            else:
                lines.append(f"{key} = {value}")
        lines.append(f"quick = {'true' if settings['quick'] else 'false'}")
        lines.append(f'runner = "{settings["runner"]}"')
        lines.append(f'profile = "{settings["profile"]}"')
        if "proposed" in settings:
            lines.append(f"proposed = {'true' if settings['proposed'] else 'false'}")
        if "note" in settings:
            lines.append(f'note = "{settings["note"]}"')
        for key, value in settings.items():
            if key in ENTRY or key in ("proposed", "note"):
                continue
            lines.append(f"{key} = {json.dumps(value)}")
        lines.append("")
    return "\n".join(lines)


def write_catalogue(directory, text, name="budgets.toml") -> Path:
    path = Path(directory) / name
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(text, encoding="utf-8")
    return path


def raw_run(values, *, run=1, errors=None, fixture=True) -> dict:
    """One raw run object as `src/measure.mjs` writes it."""
    return {
        "run": run,
        "fixture": fixture,
        "measures": {key: list(value) for key, value in values.items()},
        "errors": dict(errors or {}),
        "notes": {},
        "host": {"logicalCpus": 4},
    }


def write_runs(directory, runs) -> Path:
    directory = Path(directory)
    directory.mkdir(parents=True, exist_ok=True)
    for index, run in enumerate(runs, start=1):
        payload = dict(run)
        payload.setdefault("run", index)
        (directory / f"run-{index}.json").write_text(json.dumps(payload), encoding="utf-8")
    return directory


def all_values(**overrides):
    """A passing sample set for every budget in the fixture catalogue."""
    values = {
        "browser.download.compressed": [4_000_000],
        "browser.startup.cold": [1200.0],
        "browser.startup.warm": [300.0],
        "browser.form.open": [40.0, 42.0, 44.0],
        "browser.list.scroll.frame": [16.6, 16.7, 16.7, 16.8, 33.2],
        "browser.child-edit.key": [33.3, 34.0, 35.5],
        "browser.tabs.memory": [120_000_000],
    }
    values.update(overrides)
    return values
