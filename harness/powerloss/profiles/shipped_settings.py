"""The shipped PostgreSQL durability settings, asserted from ``SHOW`` queries (WP-PRD-05.T2).

The bundled PostgreSQL of ``spot serve`` is configured by ``spot-pg-supervisor``: ``fsync`` and
``full_page_writes`` are written as ``on`` and cannot be overridden, ``synchronous_commit`` is
left at the PostgreSQL default (``on``) for accounting data. A profile run proves that on the
**running server** (not the config file) before every cycle and fails when any value changed.

Nothing here opens a database: :func:`show_argv` builds the ``psql`` argv list and the caller
runs it. The connection string is never an argument (an argv is visible to every process on
the host): it travels in the ``PGDATABASE`` environment variable (:func:`psql_env`).
"""

from __future__ import annotations

import json
from typing import Callable, Mapping, Sequence

#: The settings a shipped host must report, with the one accepted value of each.
REQUIRED: tuple[tuple[str, str], ...] = (
    ("fsync", "on"),
    ("full_page_writes", "on"),
    ("synchronous_commit", "on"),
)
REQUIRED_NAMES = tuple(name for name, _ in REQUIRED)

#: Environment variable that holds the connection string (never printed, never an argument).
PG_URL_ENV = "SPOT_POWERLOSS_PG_URL"


class ShippedSettingsError(Exception):
    """A shipped durability setting is not what the product ships."""


def show_argv(psql: str, name: str) -> list[str]:
    """The ``psql`` argv list that runs ``SHOW <name>`` for one required setting."""
    if name not in REQUIRED_NAMES:
        raise ShippedSettingsError(f"refusing to SHOW an unlisted setting: {name!r}")
    return [psql, "-X", "-A", "-t", "-q", "-v", "ON_ERROR_STOP=1", "-c", f"SHOW {name}"]


def psql_env(base: Mapping[str, str]) -> dict[str, str]:
    """The child environment: ``PGDATABASE`` carries the URL, nothing else is added."""
    url = base.get(PG_URL_ENV, "")
    if not url:
        raise ShippedSettingsError(f"{PG_URL_ENV} is not set; cannot read the server settings")
    env = {key: value for key, value in base.items() if key != PG_URL_ENV}
    env["PGDATABASE"] = url
    return env


def read_settings(run: Callable[[Sequence[str]], str], psql: str = "psql") -> dict[str, str]:
    """Reads every required setting through ``run`` (argv list in, stdout out)."""
    values: dict[str, str] = {}
    for name in REQUIRED_NAMES:
        values[name] = run(show_argv(psql, name)).strip().lower()
    return values


def problems(settings: Mapping[str, object]) -> list[str]:
    """One line per required setting that is missing or not the shipped value."""
    found = []
    for name, expected in REQUIRED:
        raw = str(settings.get(name, "")).strip().lower()
        if raw != expected:
            found.append(f"{name}={raw or 'missing'} (shipped value is {expected})")
    return found


def assert_shipped(settings: Mapping[str, object]) -> None:
    """Fails (raises) unless every required setting has the shipped value."""
    found = problems(settings)
    if found:
        raise ShippedSettingsError("shipped durability settings changed: " + "; ".join(found))


def to_json(settings: Mapping[str, object]) -> str:
    """The settings as the one-line JSON the harness ``--settings-cmd`` contract expects."""
    return json.dumps({name: settings.get(name, "") for name in REQUIRED_NAMES}, sort_keys=True)
