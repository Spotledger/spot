"""The sample-app contract cases (WP-STR-07.T1).

``cases.json`` is the single source of truth for every language: the Rust contract kit
(``runner/``) reads it and drives the guest's ``on-event`` export, and the harness passes it
through unchanged. This module validates the file so a malformed case fails in the unit tests,
not halfway through a container build.

A case is one ``on-event`` call and what the world should do:

- ``name``: a stable label shown in the matrix.
- ``event``: the JSON envelope handed to ``on-event``.
- ``expect``: ``ok`` (the guest returns a string) or ``err`` (the guest returns its error record).
- ``contains`` (``ok`` only): a substring the returned string must contain.
- ``code`` (``err`` only): the ``code`` the error record must carry.
- ``message_contains`` (``err`` only): a substring the ``message`` must contain.
"""

from __future__ import annotations

import json
from pathlib import Path

VALID_EXPECT = ("ok", "err")


class CasesError(ValueError):
    """``cases.json`` is malformed."""


def load_cases(path: str | Path) -> list[dict]:
    """Reads and validates ``cases.json``; returns the cases in file order."""
    path = Path(path)
    try:
        data = json.loads(path.read_text(encoding="utf-8"))
    except FileNotFoundError as error:
        raise CasesError(f"no contract cases at {path}") from error
    except json.JSONDecodeError as error:
        raise CasesError(f"{path} is not valid JSON: {error}") from error

    if not isinstance(data, list) or not data:
        raise CasesError(f"{path}: expected a non-empty list of cases")

    names: set[str] = set()
    for index, case in enumerate(data):
        where = f"{path}: case {index}"
        if not isinstance(case, dict):
            raise CasesError(f"{where} is not an object")
        for key in ("name", "event", "expect"):
            if not isinstance(case.get(key), str) or not case[key]:
                raise CasesError(f"{where}: `{key}` must be a non-empty string")
        if case["name"] in names:
            raise CasesError(f"{where}: duplicate case name {case['name']!r}")
        names.add(case["name"])
        if case["expect"] not in VALID_EXPECT:
            raise CasesError(f"{where}: `expect` must be one of {VALID_EXPECT}")
        if case["expect"] == "err" and not case.get("code"):
            raise CasesError(f"{where}: an `err` case needs a `code`")
        try:
            envelope = json.loads(case["event"])
        except json.JSONDecodeError as error:
            raise CasesError(f"{where}: `event` is not valid JSON: {error}") from error
        if not isinstance(envelope, dict):
            raise CasesError(f"{where}: `event` must be a JSON object")
    return data


def case_names(cases: list[dict]) -> list[str]:
    """The case names, in order."""
    return [case["name"] for case in cases]
