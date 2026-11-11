#!/usr/bin/env python3
"""Enforce versioned public contract changes (G6)."""

from __future__ import annotations

import argparse
import os
import re
import subprocess
import sys
from dataclasses import dataclass
from pathlib import Path


VERSION_PATTERN = re.compile(r"^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$")


@dataclass(frozen=True)
class Change:
    status: str
    paths: tuple[str, ...]


def git(root: Path, *args: str, check: bool = True) -> subprocess.CompletedProcess[str]:
    result = subprocess.run(["git", *args], cwd=root, capture_output=True, text=True)
    if check and result.returncode:
        raise RuntimeError(result.stderr.strip() or f"git {' '.join(args)} failed")
    return result


def parse_changes(data: bytes) -> list[Change]:
    fields = data.decode("utf-8", errors="surrogateescape").split("\0")
    changes: list[Change] = []
    i = 0
    while i < len(fields) and fields[i]:
        status = fields[i]
        i += 1
        count = 2 if status.startswith(("R", "C")) else 1
        paths = tuple(fields[i : i + count])
        if len(paths) != count or any(not path for path in paths):
            raise ValueError("could not parse git diff name-status output")
        changes.append(Change(status[0], paths))
        i += count
    return changes


def changed_paths(changes: list[Change]) -> set[str]:
    return {path for change in changes for path in change.paths}


def contract_name(path: str) -> str | None:
    pieces = Path(path).as_posix().split("/")
    return pieces[1] if len(pieces) >= 3 and pieces[0] == "contracts" else None


def read_at(root: Path, ref: str, path: str) -> str | None:
    result = git(root, "show", f"{ref}:{path}", check=False)
    return result.stdout if result.returncode == 0 else None


def parse_version(value: str | None) -> tuple[int, int, int] | None:
    if value is None:
        return None
    match = VERSION_PATTERN.fullmatch(value.strip())
    if not match:
        return None
    return tuple(int(part) for part in match.groups())  # type: ignore[return-value]


def breaking_bump(old: tuple[int, int, int], new: tuple[int, int, int]) -> bool:
    return new[0] > old[0] or (old[0] == 0 and new[1] > old[1])


def _adr_mentions_contract(root: Path, ref: str, changes: list[Change], name: str) -> bool:
    needle_path = f"contracts/{name}"
    for change in changes:
        if change.status not in {"A", "M", "R", "C"}:
            continue
        for path in change.paths[-1:]:
            if path.startswith("docs/adr/"):
                content = read_at(root, ref, path)
                if content is not None and (needle_path in content or name in content):
                    return True
    return False


def _base_has_contract(root: Path, base_ref: str, name: str) -> bool:
    result = git(root, "ls-tree", "-r", "--name-only", base_ref, "--", f"contracts/{name}")
    return bool(result.stdout.strip())


def check_contract(root: Path, base_ref: str, head_ref: str, name: str,
                   changes: list[Change], report: list[str]) -> None:
    directory = f"contracts/{name}"
    version_path = f"{directory}/VERSION"
    base_exists = _base_has_contract(root, base_ref, name)
    head_version_text = read_at(root, head_ref, version_path)

    if not base_exists and head_version_text is None:
        report.append(f"{directory}: new contract must contain VERSION")
        return
    if head_version_text is None:
        report.append(f"{directory}: contract directory was removed or has no VERSION")
        return

    new_version = parse_version(head_version_text)
    if new_version is None:
        report.append(f"{version_path}: VERSION must be MAJOR.MINOR.PATCH")
        return
    if not base_exists:
        return

    base_version_text = read_at(root, base_ref, version_path)
    old_version = parse_version(base_version_text)
    if old_version is None:
        report.append(f"{version_path}: base VERSION is missing or is not MAJOR.MINOR.PATCH")
        return
    if version_path not in changed_paths(changes):
        report.append(f"{version_path}: contract changes require a VERSION change")
        return
    if new_version <= old_version:
        report.append(f"{version_path}: version must increase from {base_version_text.strip()} to {head_version_text.strip()}")
        return
    if breaking_bump(old_version, new_version) and not _adr_mentions_contract(root, head_ref, changes, name):
        report.append(f"{version_path}: breaking version bump requires an added or modified docs/adr/ file mentioning {directory} or {name}")


def run(root: Path, base: str | None = None) -> int:
    if not (root / "contracts").is_dir():
        print("G6: no contracts/ directory, nothing to check")
        return 0
    try:
        explicit_base = base or os.environ.get("GITHUB_BASE_REF")
        if explicit_base:
            base_ref = explicit_base if explicit_base.startswith("origin/") else f"origin/{explicit_base}"
            diff_spec = f"{base_ref}...HEAD"
        elif git(root, "branch", "--show-current").stdout.strip() == "main":
            diff_spec = "HEAD~1..HEAD"
            base_ref = "HEAD~1"
        else:
            base_ref = "origin/main"
            diff_spec = f"{base_ref}...HEAD"
        diff = git(root, "diff", "--name-status", "-z", "--find-renames", diff_spec)
        changes = parse_changes(diff.stdout.encode("utf-8", errors="surrogateescape"))
        names = sorted({name for path in changed_paths(changes) if (name := contract_name(path))})
        if not names:
            print("G6: no public contract changes")
            return 0
        violations: list[str] = []
        for name in names:
            check_contract(root, base_ref, "HEAD", name, changes, violations)
    except (OSError, RuntimeError, ValueError) as exc:
        print(f"G6: {exc}", file=sys.stderr)
        return 2
    for violation in violations:
        print(f"G6: {violation}")
    if not violations:
        print("G6: contract changes are versioned and documented")
    return 1 if violations else 0


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--base", help="base branch/ref; branch names resolve to origin/<name>")
    parser.add_argument("workspace", nargs="?", type=Path, default=Path.cwd())
    args = parser.parse_args()
    return run(args.workspace.resolve(), args.base)


if __name__ == "__main__":
    raise SystemExit(main())
