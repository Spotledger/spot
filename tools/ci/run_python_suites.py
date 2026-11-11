#!/usr/bin/env python3
"""Discover and run every Python unittest suite under tools/ and harness/.

CI gate (WP-PIP-03.T1): every directory named ``tests`` that holds at least one
``test_*.py`` file is discovered automatically, so a new suite is covered the
day it lands without anyone editing the workflow. Each suite runs as its own
``python -m unittest discover`` subprocess from the repository root, so one
suite's imports cannot leak into another, with a per-suite timeout.

Skips are declared per OS in ``tools/ci/skip.toml``; every entry needs a
reason and must match a suite the runner discovers, or the run fails.
"""

from __future__ import annotations

import argparse
import fnmatch
import os
import re
import subprocess
import sys
import time
import tomllib
from dataclasses import dataclass
from pathlib import Path
from typing import Sequence

REPO_ROOT = Path(__file__).resolve().parents[2]
SEARCH_ROOTS = ("tools", "harness")
SUITE_DIR_NAME = "tests"
TEST_FILE_GLOB = "test_*.py"
FIXTURE_COMPONENTS = frozenset({"corpus", "fixtures", "data"})
SKIP_FILE_RELPATH = Path("tools") / "ci" / "skip.toml"
KNOWN_OS_KEYS = ("linux", "macos", "windows")
DEFAULT_TIMEOUT_SECONDS = 10 * 60

PASS = "PASS"
FAIL = "FAIL"
SKIP = "SKIP"
TIMEOUT = "TIMEOUT"
FAILING_STATUSES = (FAIL, TIMEOUT)

_RAN_LINE = re.compile(r"^Ran (\d+) tests? in", re.MULTILINE)


class SkipConfigError(Exception):
    """The skip.toml file is invalid: bad structure, missing reason or stale suite."""


@dataclass(frozen=True)
class Suite:
    """A discovered test suite: repo-relative name and absolute path."""

    name: str
    path: Path


@dataclass(frozen=True)
class SkipEntry:
    """One skip.toml entry: which suite to skip on which OS, and why."""

    suite: str
    platform: str
    reason: str


@dataclass(frozen=True)
class SuiteResult:
    """The outcome of one suite: PASS, FAIL, SKIP or TIMEOUT."""

    suite: str
    status: str
    tests: int | None
    seconds: float
    note: str = ""


def platform_key() -> str:
    """Map sys.platform to the OS keys used in skip.toml."""
    if sys.platform == "win32":
        return "windows"
    if sys.platform == "darwin":
        return "macos"
    return "linux"


def is_fixture_tree(parts: tuple[str, ...]) -> bool:
    """True when a corpus/fixtures/data component sits below a tests/ directory."""
    for index, part in enumerate(parts):
        if part == SUITE_DIR_NAME:
            if any(p in FIXTURE_COMPONENTS for p in parts[index + 1 :]):
                return True
    return False


def discover_suites(base: Path, roots: Sequence[str] = SEARCH_ROOTS) -> list[Suite]:
    """Find every ``tests`` directory under the search roots that holds test files.

    A directory qualifies when it is named ``tests`` and contains at least one
    ``test_*.py`` file directly inside it. Fixture trees (any ``tests`` directory
    reached through a ``corpus``, ``fixtures`` or ``data`` component below another
    ``tests`` directory) are excluded.
    """
    suites: list[Suite] = []
    for root in roots:
        root_path = base / root
        if not root_path.is_dir():
            continue
        for dirpath, _dirnames, filenames in os.walk(root_path):
            current = Path(dirpath)
            if current.name != SUITE_DIR_NAME:
                continue
            relative = current.relative_to(base)
            if is_fixture_tree(relative.parts):
                continue
            if not any(fnmatch.fnmatchcase(f, TEST_FILE_GLOB) for f in filenames):
                continue
            suites.append(Suite(name=relative.as_posix(), path=current))
    suites.sort(key=lambda suite: suite.name)
    return suites


def load_skips(path: Path, discovered: set[str]) -> dict[str, list[SkipEntry]]:
    """Parse skip.toml into platform -> entries, validating every rule.

    A missing file means no skips. Every entry needs a non-empty reason and must
    name a suite the runner discovers (stale entries are an error), and OS keys
    must be known, so a typo cannot silently disable a skip.
    """
    if not path.is_file():
        return {}
    try:
        with open(path, "rb") as handle:
            data = tomllib.load(handle)
    except tomllib.TOMLDecodeError as exc:
        raise SkipConfigError(f"{path}: invalid TOML: {exc}") from exc

    skips: dict[str, list[SkipEntry]] = {}
    for key, entries in data.items():
        if key not in KNOWN_OS_KEYS:
            known = ", ".join(KNOWN_OS_KEYS)
            raise SkipConfigError(f"{path}: unknown OS key {key!r}; expected one of {known}")
        if not isinstance(entries, list):
            raise SkipConfigError(f"{path}: [{key}] must be an array of tables")
        parsed: list[SkipEntry] = []
        for index, entry in enumerate(entries):
            if not isinstance(entry, dict):
                raise SkipConfigError(f"{path}: [{key}] entry {index} must be an inline table")
            suite = entry.get("suite")
            reason = entry.get("reason")
            if not isinstance(suite, str) or not suite:
                raise SkipConfigError(f"{path}: [{key}] entry {index} needs a non-empty 'suite'")
            suite = suite.replace("\\", "/")
            if not isinstance(reason, str) or not reason.strip():
                raise SkipConfigError(f"{path}: [{key}] suite {suite!r} needs a non-empty 'reason'")
            if suite not in discovered:
                raise SkipConfigError(
                    f"{path}: stale skip of {suite!r}: it does not match any discovered suite"
                )
            parsed.append(SkipEntry(suite=suite, platform=key, reason=reason))
        skips.setdefault(key, []).extend(parsed)
    return skips


def test_count(output: str) -> int | None:
    """Pull the 'Ran N tests' number out of unittest's verbose output, if present."""
    matches = _RAN_LINE.findall(output)
    return int(matches[-1]) if matches else None


def format_seconds(seconds: float) -> str:
    return f"{seconds:.2f}s"


def run_suite(suite: Suite, cwd: Path, timeout: float) -> SuiteResult:
    """Run one suite as an isolated unittest subprocess and report the result."""
    command = [
        sys.executable,
        "-m",
        "unittest",
        "discover",
        "-s",
        str(suite.path),
        "-t",
        str(suite.path),
        "-v",
    ]
    started = time.monotonic()
    try:
        completed = subprocess.run(
            command,
            cwd=cwd,
            capture_output=True,
            text=True,
            timeout=timeout,
            check=False,
        )
    except subprocess.TimeoutExpired as exc:
        seconds = time.monotonic() - started
        output = _combined_output(exc.stdout, exc.stderr)
        if output:
            print(output, end="" if output.endswith("\n") else "\n")
        note = f"timed out after {format_seconds(timeout)}"
        return SuiteResult(suite.name, TIMEOUT, test_count(output), seconds, note=note)
    seconds = time.monotonic() - started
    output = _combined_output(completed.stdout, completed.stderr)
    if output:
        print(output, end="" if output.endswith("\n") else "\n")
    status = PASS if completed.returncode == 0 else FAIL
    return SuiteResult(suite.name, status, test_count(output), seconds)


def _combined_output(stdout: object, stderr: object) -> str:
    """Join captured subprocess output, decoding bytes the text mode missed.

    On a timeout, subprocess may hand back bytes even in text mode (a known
    CPython quirk on the POSIX path), so decode defensively.
    """

    def decode(part: object) -> str:
        if isinstance(part, bytes):
            return part.decode(errors="replace")
        return part or ""

    return decode(stdout) + decode(stderr)


def run_all(
    suites: Sequence[Suite],
    skips: dict[str, list[SkipEntry]],
    cwd: Path,
    timeout: float,
) -> list[SuiteResult]:
    """Run every suite on this platform, honouring the per-OS skips."""
    reasons = {entry.suite: entry.reason for entry in skips.get(platform_key(), [])}
    results: list[SuiteResult] = []
    total = len(suites)
    for index, suite in enumerate(suites, start=1):
        reason = reasons.get(suite.name)
        if reason is not None:
            results.append(SuiteResult(suite.name, SKIP, None, 0.0, note=reason))
            continue
        print(f"--- suite [{index}/{total}] {suite.name} ---", flush=True)
        results.append(run_suite(suite, cwd=cwd, timeout=timeout))
    return results


def render_table(results: Sequence[SuiteResult]) -> str:
    """Plain-text summary table: suite, result, test count, duration, note."""
    headers = ("suite", "result", "tests", "time", "note")
    rows = [
        (
            result.suite,
            result.status,
            "-" if result.tests is None else str(result.tests),
            format_seconds(result.seconds),
            result.note,
        )
        for result in results
    ]
    widths = [
        max(len(headers[column]), *(len(row[column]) for row in rows)) if rows else len(headers[column])
        for column in range(len(headers))
    ]
    lines = ["  ".join(header.ljust(widths[c]) for c, header in enumerate(headers)).rstrip()]
    lines.append("  ".join("-" * width for width in widths))
    for row in rows:
        lines.append("  ".join(cell.ljust(widths[c]) for c, cell in enumerate(row)).rstrip())
    return "\n".join(lines)


def render_markdown_table(results: Sequence[SuiteResult]) -> str:
    """The same summary as a Markdown table, for $GITHUB_STEP_SUMMARY."""
    lines = [
        "| suite | result | tests | time | note |",
        "| --- | --- | --- | --- | --- |",
    ]
    for result in results:
        tests = "-" if result.tests is None else str(result.tests)
        note = result.note.replace("|", "\\|")
        lines.append(
            f"| {result.suite} | {result.status} | {tests} | {format_seconds(result.seconds)} | {note} |"
        )
    return "\n".join(lines)


def write_step_summary(results: Sequence[SuiteResult]) -> None:
    """Append the summary table to $GITHUB_STEP_SUMMARY when CI provides it."""
    target = os.environ.get("GITHUB_STEP_SUMMARY")
    if not target:
        return
    with open(target, "a", encoding="utf-8") as handle:
        handle.write("\n### Python test suites\n\n")
        handle.write(render_markdown_table(results))
        handle.write("\n")


def summarize(results: Sequence[SuiteResult]) -> str:
    counts = {status: 0 for status in (PASS, FAIL, SKIP, TIMEOUT)}
    for result in results:
        counts[result.status] += 1
    seconds = sum(result.seconds for result in results)
    return (
        f"{len(results)} suites: "
        f"{counts[PASS]} passed, {counts[FAIL]} failed, "
        f"{counts[TIMEOUT]} timed out, {counts[SKIP]} skipped "
        f"in {format_seconds(seconds)}"
    )


def parse_args(argv: Sequence[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Discover and run every Python unittest suite under tools/ and harness/."
    )
    parser.add_argument("--list", action="store_true", help="print the discovered suites and exit")
    parser.add_argument(
        "--timeout",
        type=float,
        default=DEFAULT_TIMEOUT_SECONDS,
        metavar="SECONDS",
        help=f"per-suite timeout in seconds (default: {DEFAULT_TIMEOUT_SECONDS})",
    )
    parser.add_argument(
        "--base",
        type=Path,
        default=REPO_ROOT,
        help=argparse.SUPPRESS,  # repository root; overridable for tests
    )
    return parser.parse_args(argv)


def main(argv: Sequence[str] | None = None) -> int:
    args = parse_args(argv)
    base = args.base.resolve()

    suites = discover_suites(base)
    if not suites:
        roots = ", ".join(SEARCH_ROOTS)
        print(
            f"error: no Python test suites found under {roots}/; a broken discovery must not pass",
            file=sys.stderr,
        )
        return 2

    if args.list:
        for suite in suites:
            print(suite.name)
        return 0

    try:
        skips = load_skips(base / SKIP_FILE_RELPATH, discovered={suite.name for suite in suites})
    except SkipConfigError as exc:
        print(f"error: {exc}", file=sys.stderr)
        return 2

    results = run_all(suites, skips, cwd=base, timeout=args.timeout)

    print()
    print(render_table(results))
    failed = [result for result in results if result.status in FAILING_STATUSES]
    if failed:
        print(f"FAILED suites: {', '.join(result.suite for result in failed)}")
    print(summarize(results))
    write_step_summary(results)
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())