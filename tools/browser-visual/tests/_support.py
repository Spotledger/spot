"""Shared helpers for the browser visual tests.

Puts `tools/browser-visual/src` on `sys.path` so `import compare` and `import png` work both under
`python -m unittest discover -s tools/browser-visual/tests` and when a single file is run directly.
No network and no browser here: the comparator is pure Python.
"""
from __future__ import annotations

import json
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
SRC = ROOT / "src"
for candidate in (ROOT, SRC):
    if str(candidate) not in sys.path:
        sys.path.insert(0, str(candidate))

import compare  # noqa: E402  (import after the path fix on purpose)
import png  # noqa: E402

SCENARIOS = ROOT / "scenarios.json"


def scenario_document():
    return json.loads(SCENARIOS.read_text(encoding="utf-8"))


def capture_name(scenario_id, width, height):
    return f"{scenario_id}-{width}x{height}"
