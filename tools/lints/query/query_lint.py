#!/usr/bin/env python3
"""Reject SQL statements and database drivers outside storage crates (G5)."""

from __future__ import annotations

import argparse
import fnmatch
import re
import sys
import tomllib
from dataclasses import dataclass
from pathlib import Path


HERE = Path(__file__).resolve().parent
SQL_PATTERNS = (
    ("SELECT … FROM", re.compile(r"\bSELECT\b[\s\S]*?\bFROM\b", re.IGNORECASE)),
    ("INSERT INTO", re.compile(r"\bINSERT\s+INTO\b", re.IGNORECASE)),
    ("UPDATE … SET", re.compile(r"\bUPDATE\b[\s\S]*?\bSET\b", re.IGNORECASE)),
    ("DELETE FROM", re.compile(r"\bDELETE\s+FROM\b", re.IGNORECASE)),
    ("CREATE TABLE", re.compile(r"\bCREATE\s+TABLE\b", re.IGNORECASE)),
    ("ALTER TABLE", re.compile(r"\bALTER\s+TABLE\b", re.IGNORECASE)),
    ("DROP TABLE", re.compile(r"\bDROP\s+TABLE\b", re.IGNORECASE)),
    ("WITH … AS (", re.compile(r"\bWITH\b[\s\S]*?\bAS\s*\(", re.IGNORECASE)),
    ("LISTEN statement", re.compile(r"\bLISTEN\s+[\w\"]+", re.IGNORECASE)),
    ("NOTIFY statement", re.compile(r"\bNOTIFY\s+[\w\"]+", re.IGNORECASE)),
)
DRIVERS = "sqlx|tokio_postgres|postgres|diesel|sea_orm|deadpool_postgres|rusqlite"
RAW_STRING_PATTERN = re.compile(r"(?:br|r)(#+)?\"")
DRIVER_PATH_PATTERN = re.compile(rf"(?<![\w:.])({DRIVERS})\b(?=\s*::)")
ROOT_DRIVER_PATH_PATTERN = re.compile(rf"(?<![\w.])::({DRIVERS})\b(?=\s*::)")
DRIVER_IMPORT_PATTERN = re.compile(
    rf"\b(?:use\s+|extern\s+crate\s+)({DRIVERS})\b(?=\s*(?:::|;|,|as\b))"
)


@dataclass(frozen=True)
class Literal:
    value: str
    start: int


def _mask(text: str) -> str:
    """Hide comments and strings, retaining offsets and line breaks."""
    return "".join("\n" if char == "\n" else " " for char in text)


def tokenize(source: str) -> tuple[str, list[Literal]]:
    """Return source with comments/strings blanked and the string literals found."""
    code = list(source)
    literals: list[Literal] = []
    length = len(source)
    i = 0

    def blank(start: int, end: int) -> None:
        code[start:end] = _mask(source[start:end])

    def char_end(start: int, quote_offset: int) -> int | None:
        """Return the end of a Rust char literal, or None for a lifetime/label."""
        quote = start + quote_offset
        body = quote + 1
        if body >= length or source[body] in "\r\n":
            return None
        if source[body] == "\\":
            j = body + 1
            if j >= length or source[j] in "\r\n":
                return None
            if source[j] == "u" and j + 1 < length and source[j + 1] == "{":
                close_brace = source.find("}", j + 2)
                if close_brace < 0 or "\n" in source[j:close_brace]:
                    return None
                j = close_brace + 1
            else:
                j += 1
            return j + 1 if j < length and source[j] == "'" else None
        return body + 2 if body + 1 < length and source[body + 1] == "'" else None

    while i < length:
        if source.startswith("//", i):
            end = source.find("\n", i)
            if end < 0:
                end = length
            blank(i, end)
            i = end
            continue
        if source.startswith("/*", i):
            start = i
            depth = 1
            i += 2
            while i < length and depth:
                if source.startswith("/*", i):
                    depth += 1
                    i += 2
                elif source.startswith("*/", i):
                    depth -= 1
                    i += 2
                else:
                    i += 1
            blank(start, i)
            continue

        # Raw string literals: r"...", r#"..."#, br##"..."##.
        raw = RAW_STRING_PATTERN.match(source, i)
        if raw:
            start = i
            hashes = raw.group(1) or ""
            body_start = raw.end()
            terminator = '"' + hashes
            end_quote = source.find(terminator, body_start)
            end = length if end_quote < 0 else end_quote + len(terminator)
            literals.append(Literal(source[body_start : end_quote if end_quote >= 0 else length], start))
            blank(start, end)
            i = end
            continue

        # Character and byte-character literals need masking so quotes inside
        # them do not affect later string/comment detection. Lifetimes and labels
        # have no closing quote immediately after their identifier and stay code.
        char_quote_offset = 1 if source.startswith("b'", i) else 0 if source[i] == "'" else None
        if char_quote_offset is not None:
            end = char_end(i, char_quote_offset)
            if end is not None:
                blank(i, end)
                i = end
                continue

        # Ordinary and byte strings, with Rust escape handling.
        quote = i + 1 if source.startswith('b"', i) else i
        if source[quote] == '"':
            start = i
            body_start = quote + 1
            j = body_start
            while j < length:
                if source[j] == "\\":
                    j += 2
                elif source[j] == '"':
                    break
                else:
                    j += 1
            end = min(j + 1, length)
            literals.append(Literal(source[body_start:j], start))
            blank(start, end)
            i = end
            continue
        i += 1

    return "".join(code), literals


def load_allowlist(path: Path) -> list[dict[str, str]]:
    if not path.exists():
        return []
    with path.open("rb") as stream:
        data = tomllib.load(stream)
    entries = data.get("allow", [])
    if not isinstance(entries, list):
        raise ValueError("allow.toml must contain [[allow]] entries")
    for entry in entries:
        if not isinstance(entry, dict) or not all(key in entry for key in ("path", "pattern", "reason")):
            raise ValueError("every allow.toml entry requires path, pattern and reason")
        if not all(isinstance(entry[key], str) and entry[key].strip() for key in ("path", "pattern", "reason")):
            raise ValueError("allow.toml path, pattern and reason must be non-empty strings")
        re.compile(entry["pattern"])
    return entries


def _is_allowed(path: str, what: str, allowlist: list[dict[str, str]]) -> bool:
    return any(
        fnmatch.fnmatchcase(path, entry["path"]) and re.search(entry["pattern"], what)
        for entry in allowlist
    )


def _scan_file(path: Path, root: Path, allowlist: list[dict[str, str]]) -> list[tuple[str, int, str]]:
    source = path.read_text(encoding="utf-8")
    code, literals = tokenize(source)
    relative = path.relative_to(root).as_posix()
    violations: list[tuple[str, int, str]] = []

    for literal in literals:
        for label, pattern in SQL_PATTERNS:
            match = pattern.search(literal.value)
            if match:
                line = source.count("\n", 0, literal.start) + 1 + literal.value.count("\n", 0, match.start())
                what = f"SQL {label} statement"
                if not _is_allowed(relative, what, allowlist):
                    violations.append((relative, line, what))
                break

    driver_matches = [
        (match.start(1), match.group(1))
        for pattern in (DRIVER_PATH_PATTERN, ROOT_DRIVER_PATH_PATTERN, DRIVER_IMPORT_PATTERN)
        for match in pattern.finditer(code)
    ]
    for offset, name in sorted(set(driver_matches)):
        line = code.count("\n", 0, offset) + 1
        what = f"database driver crate path: {name}"
        if not _is_allowed(relative, what, allowlist):
            violations.append((relative, line, what))
    return violations


def run(root: Path, allow_path: Path | None = None) -> int:
    crates = root / "crates"
    if not crates.is_dir():
        print("G5: no crates/ directory, nothing to check")
        return 0
    try:
        allowlist = load_allowlist(allow_path or HERE / "allow.toml")
        files = sorted(
            path for path in crates.rglob("*.rs")
            if not path.relative_to(root).as_posix().startswith(("crates/framework/spot-storage/", "crates/backends/"))
        )
        violations = [violation for path in files for violation in _scan_file(path, root, allowlist)]
    except (OSError, ValueError, re.error) as exc:
        print(f"G5: {exc}", file=sys.stderr)
        return 2
    for path, line, what in violations:
        print(f"G5: {path}:{line}: {what}")
    if not violations:
        print("G5: no backend-specific queries outside storage crates")
    return 1 if violations else 0


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("workspace", nargs="?", type=Path, default=HERE.parents[2])
    args = parser.parse_args()
    return run(args.workspace.resolve())


if __name__ == "__main__":
    raise SystemExit(main())
