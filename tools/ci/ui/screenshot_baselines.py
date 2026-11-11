#!/usr/bin/env python3
"""Verify the per-platform screenshot baseline inventory (WP-UI-16.T2).

Font rasterization differs per platform, so every screenshot case has one reviewed
baseline per platform under ``Baselines/<platform>/``. There is no fallback: a case
that exists for one platform must exist for every platform unless
``Baselines/exceptions.json`` names it with a written reason. A missing baseline is
never a pass; this tool fails with the exact command that creates it.

Read-only. Generation is done by the test helper under ``UPDATE_BASELINES=1`` so the
committed PNG is always produced by a real render on the platform that will compare
it. This tool then proves the set is complete.

    python tools/ci/ui/screenshot_baselines.py --platform linux --check
    python tools/ci/ui/screenshot_baselines.py --platform windows --list
"""
from __future__ import annotations

import argparse
from dataclasses import dataclass
import json
from pathlib import Path
import re
import sys

PLATFORMS = ('linux', 'windows', 'macos')
DEFAULT_ROOT = Path('clients/avalonia/tests/Spotledger.Client.TestSupport.Tests/Baselines')
# `<case>-<width>x<height>-<density>-<theme>-<direction>[-<suffix>].png`, exactly as
# ScreenshotAssert builds the stem: state, logical size, density, theme, direction.
CASE_NAME = re.compile(r'^[A-Za-z0-9][A-Za-z0-9_-]*-\d+x\d+-(comfortable|compact)-(light|dark)-(ltr|rtl)(-[a-z0-9]+)?$')
EXCEPTIONS = 'exceptions.json'
UPDATE_COMMAND = (
    'UPDATE_BASELINES=1 dotnet test clients/avalonia/tests/Spotledger.Client.TestSupport.Tests'
)


class BaselineError(ValueError):
    """Invalid inventory; never a pass and never a silent skip."""


@dataclass(frozen=True)
class Inventory:
    """The cases each platform commits, and the approved single-platform exceptions."""

    by_platform: dict
    cases: frozenset
    exceptions: dict

    def missing(self, platform):
        if platform not in PLATFORMS:
            raise BaselineError(f'Unknown platform {platform!r}; expected one of {", ".join(PLATFORMS)}')
        return tuple(sorted(case for case in self.cases
                            if case not in self.by_platform.get(platform, frozenset())
                            and (platform, case) not in self.exceptions))

    def unexpected(self, platform):
        """Baselines that exist here but are not part of the committed matrix at all."""
        return tuple(sorted(self.by_platform.get(platform, frozenset()) - self.cases))


def load_exceptions(root):
    path = _as_path(root) / EXCEPTIONS
    if not path.is_file():
        return {}
    try:
        data = json.loads(path.read_text(encoding='utf-8'))
    except (OSError, ValueError) as error:
        raise BaselineError(f'{path}: {error}') from error
    entries = data.get('exceptions') if isinstance(data, dict) else None
    if not isinstance(entries, list):
        raise BaselineError(f'{path}: expected {{"exceptions": [...]}}')
    exceptions = {}
    for entry in entries:
        if not isinstance(entry, dict):
            raise BaselineError(f'{path}: every exception is an object')
        platform, case, reason = entry.get('platform'), entry.get('case'), entry.get('reason')
        if platform not in PLATFORMS:
            raise BaselineError(f'{path}: exception platform {platform!r} is not one of {PLATFORMS}')
        if not isinstance(case, str) or not CASE_NAME.match(case):
            raise BaselineError(f'{path}: exception case {case!r} is not a screenshot case name')
        if not isinstance(reason, str) or not reason.strip():
            raise BaselineError(f'{path}: exception {platform}/{case} needs a written reason')
        if (platform, case) in exceptions:
            raise BaselineError(f'{path}: duplicate exception {platform}/{case}')
        exceptions[(platform, case)] = reason.strip()
    return exceptions


def _as_path(root):
    return root if isinstance(root, Path) else Path(root)


def load(root=DEFAULT_ROOT):
    """Read every platform directory; reject stray file names and empty inventories."""
    root = _as_path(root)
    if not root.is_dir():
        raise BaselineError(f'{root}: no baseline root; run the screenshot tests once')
    by_platform = {}
    for platform in PLATFORMS:
        directory = root / platform
        cases = set()
        if directory.is_dir():
            for path in sorted(directory.iterdir()):
                if path.is_dir():
                    raise BaselineError(f'{path}: platform directories hold PNG files only')
                if path.suffix.lower() != '.png':
                    raise BaselineError(f'{path}: only .png baselines are committed')
                if not CASE_NAME.match(path.stem):
                    raise BaselineError(f'{path}: name is not <case>-<w>x<h>-<density>-<theme>-<direction>.png')
                cases.add(path.stem)
        by_platform[platform] = frozenset(cases)
    cases = frozenset().union(*by_platform.values()) if by_platform else frozenset()
    if not cases:
        raise BaselineError(f'{root}: no baselines at all; commit a reviewed render before gating')
    return Inventory(by_platform=by_platform, cases=cases, exceptions=load_exceptions(root))


def verify(inventory, platform):
    missing = inventory.missing(platform)
    if missing:
        raise BaselineError(
            f'{len(missing)} baseline(s) missing for {platform}: {", ".join(missing)}\n'
            f'Create them on a {platform} runner and review every PNG before committing:\n'
            f'  {UPDATE_COMMAND}\n'
            f'Then rerun: python tools/ci/ui/screenshot_baselines.py --platform {platform} --check\n'
            'Never copy another platform\'s baseline.')
    stray = inventory.unexpected(platform)
    if stray:
        raise BaselineError(f'{platform}: baselines outside the committed matrix: {", ".join(stray)}')
    return len(inventory.by_platform[platform])


def main(argv=None):
    parser = argparse.ArgumentParser(description='Verify the per-platform screenshot baseline inventory.')
    parser.add_argument('--platform', required=True, choices=PLATFORMS)
    parser.add_argument('--root', default=DEFAULT_ROOT, type=Path)
    parser.add_argument('--check', action='store_true', help='fail when a baseline is missing')
    parser.add_argument('--list', action='store_true', help='print the case list for this platform')
    parser.add_argument('--generate-command', action='store_true', help='print the update command')
    arguments = parser.parse_args(argv)
    try:
        if arguments.generate_command:
            print(UPDATE_COMMAND)
            return 0
        inventory = load(arguments.root)
        if arguments.list:
            for case in sorted(inventory.by_platform[arguments.platform]):
                print(case)
            return 0
        if arguments.check:
            count = verify(inventory, arguments.platform)
            print(f'screenshot_baselines: {arguments.platform} has all {count} committed cases')
            return 0
    except BaselineError as error:
        print(f'screenshot_baselines: {error}', file=sys.stderr)
        return 1
    parser.error('choose --check, --list or --generate-command')
    return 2


if __name__ == '__main__':
    raise SystemExit(main())
