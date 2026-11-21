#!/usr/bin/env python3
"""Object names a backend crate creates must start with `spot_` (WP-CON-05.T4).

`spot_pg_migrate` refuses, as a destructive change, every live table whose name
does not start with `spot_` and that the catalogue does not know
(`crates/backends/spot-pg-migrate/src/plan.rs`, `live.rs`). A backend crate that
creates `sso_replay` instead of `spot_sso_replay` therefore breaks `spot
migrate`, `install-app` and runtime customise the first time the host creates
its objects. This gate reads the SQL string literals under
`crates/backends/*/src/` and fails on an object name without the prefix.

Only string-literal text is read: `//`, `///`, `//!` and `/* */` comments are
ignored, and ordinary, raw (`r"…"`) and raw-hash (`r#"…"#`) literals are all
scanned. The tokenizer mirrors `tools/lints/query/query_lint.py`.

Statement kinds checked (`<name>` after an optional quoted `schema.` qualifier):

  CREATE [UNLOGGED] TABLE [IF NOT EXISTS] <name>
  CREATE [UNIQUE] INDEX [CONCURRENTLY] [IF NOT EXISTS] <name> [ON <table>]
  CREATE SEQUENCE [IF NOT EXISTS] <name>
  CREATE [OR REPLACE] [MATERIALIZED] VIEW [IF NOT EXISTS] <name>
  CREATE TYPE <name>
  [ADD] CONSTRAINT <name>
  REFERENCES <name>
  ALTER TABLE <table> RENAME TO <name>

Exempt: `CREATE TEMP`/`CREATE TEMPORARY` tables, every object under the
`spot_control` schema (and an index whose `ON` table is in it), and the names
that are Rust format placeholders (`{TABLE}`, `{name}`): those are counted as
*unchecked* and printed with their `file:line` instead of failing.

The run exits 0 when the tree is clean, 1 on a finding or an allow-list problem
and 2 when an input cannot be read.

Standard library only; the gate never reads the network.
"""

from __future__ import annotations

import argparse
import io
import re
import sys
import tomllib
from dataclasses import dataclass, field
from pathlib import Path

HERE = Path(__file__).resolve().parent
PREFIX = "spot_"
# The control-plane schemas. The site-database migrator reads only
# `current_schema()` (`spot-pg-migrate/src/live.rs`), so an object in one of
# these schemas is never seen by its prefix rule and is outside this gate.
# `spot_control` is the tenant/control schema; `spot_migration_control` is the
# migration-apply crate's own schema, which the first clean scan found.
CONTROL_SCHEMAS = ("spot_control", "spot_migration_control")
BACKENDS = Path("crates/backends")
TAG = "TABLE-PREFIX"

# A name token: a Rust format placeholder, a quoted SQL identifier or a bare
# identifier. A qualified name is `schema.object`.
_TOKEN = r"\{[^{}\n]*\}|\"[^\"\n]*\"|[A-Za-z_][A-Za-z0-9_$]*"


def _qualified(prefix: str) -> str:
    """A name pattern whose groups are `<prefix>_schema` and `<prefix>_object`."""
    return rf"(?P<{prefix}_schema>{_TOKEN})(?:\s*\.\s*(?P<{prefix}_object>{_TOKEN}))?"


_I = re.IGNORECASE
# `CREATE TEMP TABLE` / `CREATE TEMPORARY TABLE` never match: after CREATE the
# only optional word is UNLOGGED, so a session-scratch table is exempt by shape.
CREATE_TABLE = re.compile(rf"\bCREATE\s+(?:UNLOGGED\s+)?TABLE\s+(?:IF\s+NOT\s+EXISTS\s+)?{_qualified('name')}", _I)
CREATE_INDEX = re.compile(
    rf"\bCREATE\s+(?:UNIQUE\s+)?INDEX\s+(?:(?:CONCURRENTLY|IF\s+NOT\s+EXISTS)\s+)*{_qualified('name')}",
    _I,
)
CREATE_SEQUENCE = re.compile(rf"\bCREATE\s+SEQUENCE\s+(?:IF\s+NOT\s+EXISTS\s+)?{_qualified('name')}", _I)
CREATE_VIEW = re.compile(
    rf"\bCREATE\s+(?:OR\s+REPLACE\s+)?(?:MATERIALIZED\s+)?VIEW\s+(?:IF\s+NOT\s+EXISTS\s+)?{_qualified('name')}",
    _I,
)
CREATE_TYPE = re.compile(rf"\bCREATE\s+TYPE\s+{_qualified('name')}", _I)
CONSTRAINT = re.compile(rf"\b(?:ADD\s+)?CONSTRAINT\s+{_qualified('name')}", _I)
REFERENCES = re.compile(rf"\bREFERENCES\s+{_qualified('name')}", _I)
RENAME = re.compile(
    rf"\bALTER\s+TABLE\s+(?:IF\s+EXISTS\s+)?(?:ONLY\s+)?{_qualified('old')}\s+RENAME\s+TO\s+{_qualified('name')}",
    _I,
)
INDEX_ON = re.compile(rf"\bON\s+(?:ONLY\s+)?{_qualified('table')}", _I)
_TABLE_STATEMENT = re.compile(r"\b(?:CREATE\s+(?:UNLOGGED\s+)?TABLE|ALTER\s+TABLE)\b", _I)

# (pattern, kind label) for the statements whose first name is the object name.
# `CREATE INDEX` is handled on its own because its exemption reads the `ON` table.
STATEMENTS = (
    (CREATE_TABLE, "table"),
    (CREATE_SEQUENCE, "sequence"),
    (CREATE_VIEW, "view"),
    (CREATE_TYPE, "type"),
    (CONSTRAINT, "constraint"),
    (REFERENCES, "foreign key to table"),
)

RAW_STRING_PATTERN = re.compile(r"(?:br|r)(#+)?\"")
_CONTEXT_ONLY = {"constraint", "foreign key to table"}


@dataclass(frozen=True)
class Literal:
    """A string literal: its value and the offset of its opening quote."""

    value: str
    start: int


@dataclass(frozen=True)
class Finding:
    """One object name that must be renamed."""

    path: str
    line: int
    kind: str
    name: str
    crate: str

    @property
    def message(self) -> str:
        return f"{self.kind} `{self.name}` does not start with `{PREFIX}`"


@dataclass(frozen=True)
class Unchecked:
    """One object name the gate could not read (a Rust format placeholder)."""

    path: str
    line: int
    kind: str
    name: str


@dataclass(frozen=True)
class AllowEntry:
    """One allow file: a crate that still creates unprefixed objects."""

    crate: str
    reason: str
    rename_task: str
    file: str


@dataclass
class Scan:
    """Everything one run found."""

    findings: list[Finding] = field(default_factory=list)
    unchecked: list[Unchecked] = field(default_factory=list)
    prefixed: int = 0
    exempt: int = 0
    checked: int = 0
    files: int = 0


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
        # them do not affect later string/comment detection.
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


def _split(match: re.Match[str], prefix: str) -> tuple[str | None, str]:
    """Return (schema, object) for a name match; schema is None when unqualified."""
    schema = match.group(f"{prefix}_schema")
    obj = match.group(f"{prefix}_object")
    if obj is None:
        return None, schema
    return schema, obj


def _strip_quotes(token: str) -> str:
    return token.strip().strip('"').strip()


def _is_placeholder(name: str) -> bool:
    return name.startswith("{")


def _is_control(schema: str | None) -> bool:
    return schema is not None and _strip_quotes(schema).lower() in CONTROL_SCHEMAS


def _crate_of(relative: str) -> str:
    """`crates/backends/<crate>/src/x.rs` -> `<crate>`."""
    parts = relative.split("/")
    return parts[2] if len(parts) > 3 and parts[0] == "crates" and parts[1] == "backends" else ""


def _in_table_statement(text: str, position: int) -> bool:
    """True when the token at `position` sits in a CREATE TABLE/ALTER TABLE statement.

    `CONSTRAINT` and `REFERENCES` only mean an object name inside a table
    definition, so a sentence such as "the constraint violation" in a prose
    literal is not read as one.
    """
    start = text.rfind(";", 0, position) + 1
    return _TABLE_STATEMENT.search(text, start, position) is not None


def _statement_end(text: str, position: int) -> int:
    end = text.find(";", position)
    return len(text) if end < 0 else end


_CFG_TEST = re.compile(r"#\s*\[\s*cfg\s*\(\s*test\s*\)\s*\]")


def test_spans(code: str) -> list[tuple[int, int]]:
    """Spans of `#[cfg(test)]` items in masked code, so unit-test fixtures are skipped.

    `tests/`, `benches/` and `examples/` are already out of scope; an inline
    `#[cfg(test)] mod tests` is test code too, and its assertion strings name
    scratch tables (the same reason the brief excludes `tests/`).
    """
    spans: list[tuple[int, int]] = []
    for match in _CFG_TEST.finditer(code):
        i = match.end()
        while i < len(code):
            char = code[i]
            if char == ";":
                spans.append((match.start(), i + 1))
                break
            if char == "{":
                depth = 0
                for j in range(i, len(code)):
                    if code[j] == "{":
                        depth += 1
                    elif code[j] == "}":
                        depth -= 1
                        if depth == 0:
                            spans.append((match.start(), j + 1))
                            break
                break
            i += 1
    return spans


def sql_view(text: str) -> str:
    """Replace Rust escapes with same-width filler so SQL quoting survives.

    A literal keeps its Rust escapes (`\\"{TABLE}\\"`), which would hide a quoted
    identifier from the patterns below. An escaped quote becomes a quote plus a
    space (the space is stripped again when the name is read) and every other
    escape becomes spaces, so offsets and line breaks match the literal exactly.
    """
    if "\\" not in text:
        return text
    view: list[str] = []
    i = 0
    while i < len(text):
        char = text[i]
        if char == "\\" and i + 1 < len(text):
            escaped = text[i + 1]
            if escaped == "\n":
                # A continuation: one space plus the real line break, same width.
                view.append(" ")
                view.append("\n")
            elif escaped in "\"'":
                view.append(escaped)
                view.append(" ")
            else:
                view.append(" ")
                view.append(" ")
            i += 2
            continue
        view.append(char)
        i += 1
    return "".join(view)


def _scan_literal(scan: Scan, relative: str, source: str, literal: Literal) -> None:
    text = sql_view(literal.value)
    base = source.count("\n", 0, literal.start) + 1
    crate = _crate_of(relative)

    def line_at(position: int) -> int:
        return base + text.count("\n", 0, position)

    def record(kind: str, schema: str | None, name: str, position: int) -> None:
        raw = _strip_quotes(name)
        schema_name = None if schema is None else _strip_quotes(schema)
        scan.checked += 1
        if _is_control(schema):
            scan.exempt += 1
            return
        if _is_placeholder(raw):
            scan.unchecked.append(Unchecked(relative, line_at(position), kind, raw))
            return
        if raw.lower().startswith(PREFIX):
            scan.prefixed += 1
            return
        if schema_name is not None and _is_placeholder(schema_name):
            # An unqualified-by-placeholder name could still land in a control
            # schema, so the gate cannot judge it.
            scan.unchecked.append(Unchecked(relative, line_at(position), kind, raw))
            return
        scan.findings.append(Finding(relative, line_at(position), kind, raw, crate))

    for pattern, kind in STATEMENTS:
        for match in pattern.finditer(text):
            if kind in _CONTEXT_ONLY and not _in_table_statement(text, match.start()):
                continue
            if kind == "constraint" and re.search(r"\bDROP\s+$", text[: match.start()], _I):
                # `ALTER TABLE … DROP CONSTRAINT <name>` removes an object; it
                # does not create one.
                continue
            schema, name = _split(match, "name")
            record(kind, schema, name, match.start("name_schema"))

    for match in CREATE_INDEX.finditer(text):
        # An index whose table lives in the control schema is exempt even when
        # the index name itself is unprefixed.
        schema, name = _split(match, "name")
        raw = _strip_quotes(name)
        scan.checked += 1
        if _is_control(schema):
            scan.exempt += 1
            continue
        if _is_placeholder(raw):
            scan.unchecked.append(Unchecked(relative, line_at(match.start("name_schema")), "index", raw))
            continue
        if raw.lower().startswith(PREFIX):
            scan.prefixed += 1
            continue
        start = match.end("name_schema")
        on = INDEX_ON.search(text, start, _statement_end(text, start))
        if on is not None:
            table_schema = _split(on, "table")[0]
            if _is_control(table_schema):
                scan.exempt += 1
                continue
            if table_schema is not None and _is_placeholder(_strip_quotes(table_schema)):
                scan.unchecked.append(Unchecked(relative, line_at(match.start("name_schema")), "index", raw))
                continue
        if schema is not None and _is_placeholder(_strip_quotes(schema)):
            scan.unchecked.append(Unchecked(relative, line_at(match.start("name_schema")), "index", raw))
            continue
        scan.findings.append(Finding(relative, line_at(match.start("name_schema")), "index", raw, crate))

    for match in RENAME.finditer(text):
        schema, name = _split(match, "name")
        record("renamed table", schema, name, match.start("name_schema"))


def scan_tree(root: Path) -> tuple[Scan, list[Path]]:
    """Scan every `*.rs` under `crates/backends/*/src/` (never tests/benches/examples)."""
    backends = root / BACKENDS
    scan = Scan()
    if not backends.is_dir():
        return scan, []
    files = sorted(
        path
        for crate in sorted(backends.iterdir())
        if crate.is_dir() and (crate / "src").is_dir()
        for path in (crate / "src").rglob("*.rs")
        if path.is_file()
    )
    for path in files:
        relative = path.relative_to(root).as_posix()
        source = path.read_text(encoding="utf-8")
        scan.files += 1
        code, literals = tokenize(source)
        spans = test_spans(code)
        for literal in literals:
            if any(start <= literal.start < end for start, end in spans):
                continue
            _scan_literal(scan, relative, source, literal)
    return scan, files


REQUIRED_ALLOW_KEYS = ("crate", "reason", "rename_task")


def load_allowlist(directory: Path, backends: Path) -> tuple[list[AllowEntry], list[str]]:
    """Read `allow/<crate>.toml`; return the entries and every problem with them."""
    entries: list[AllowEntry] = []
    problems: list[str] = []
    if not directory.is_dir():
        return entries, problems
    for path in sorted(directory.glob("*.toml")):
        relative = path.as_posix()
        with path.open("rb") as stream:
            data = tomllib.load(stream)
        unknown = set(data) - set(REQUIRED_ALLOW_KEYS)
        if unknown:
            problems.append(f"{relative}: unknown fields {sorted(unknown)} (allowed: {list(REQUIRED_ALLOW_KEYS)})")
        missing = [key for key in REQUIRED_ALLOW_KEYS if not isinstance(data.get(key), str) or not data.get(key, "").strip()]
        if missing:
            problems.append(f"{relative}: missing or empty {', '.join(missing)}")
            continue
        crate = data["crate"].strip()
        if path.stem != crate:
            problems.append(f"{relative}: crate = {crate!r} does not match the file name {path.stem!r}")
        if not (backends / crate).is_dir():
            problems.append(f"{relative}: crate {crate!r} has no directory under {BACKENDS.as_posix()}/")
            continue
        entries.append(AllowEntry(crate=crate, reason=data["reason"].strip(), rename_task=data["rename_task"].strip(), file=relative))
    return entries, problems


def run(root: Path, allow_dir: Path | None = None, output: io.StringIO | None = None) -> int:
    """Scan `root` and print the verdict; return the process exit code."""
    stream = output if output is not None else sys.stdout
    backends = root / BACKENDS
    if not backends.is_dir():
        print(f"{TAG}: no {BACKENDS.as_posix()}/ directory, nothing to check", file=stream)
        return 0
    try:
        entries, problems = load_allowlist(allow_dir or HERE / "allow", backends)
        scan, files = scan_tree(root)
    except (OSError, ValueError, tomllib.TOMLDecodeError) as exc:
        print(f"{TAG}: {exc}", file=sys.stderr)
        return 2

    allowed = {entry.crate for entry in entries}
    allowed_counts: dict[str, int] = {}
    failing: list[Finding] = []
    for finding in scan.findings:
        if finding.crate in allowed:
            allowed_counts[finding.crate] = allowed_counts.get(finding.crate, 0) + 1
        else:
            failing.append(finding)
    stale = [entry for entry in entries if entry.crate not in allowed_counts]

    for finding in failing:
        print(f"{TAG}: {finding.path}:{finding.line}: {finding.message}", file=stream)
    for problem in problems:
        print(f"{TAG}: allow-list: {problem}", file=stream)
    for entry in stale:
        print(
            f"{TAG}: allow-list: {entry.file}: stale entry, crate {entry.crate!r} creates no unprefixed object "
            f"(its rename task {entry.rename_task} is done: delete the file)",
            file=stream,
        )
    if allowed_counts:
        listed = ", ".join(f"{crate} ({count})" for crate, count in sorted(allowed_counts.items()))
        print(f"{TAG}: allowed (rename pending): {sum(allowed_counts.values())} finding(s) in {len(allowed_counts)} crate(s): {listed}", file=stream)
    if scan.unchecked:
        print(f"{TAG}: unchecked: {len(scan.unchecked)} object name(s) are format placeholders, not read:", file=stream)
        for unchecked in scan.unchecked:
            print(f"{TAG}: unchecked {unchecked.path}:{unchecked.line}: {unchecked.kind} name `{unchecked.name}`", file=stream)

    failed = bool(failing or problems or stale)
    if failed:
        print(f"{TAG}: {len(failing)} unprefixed object(s) in {len({f.crate for f in failing})} crate(s); every name must start with `{PREFIX}`", file=stream)
        return 1
    print(
        f"{TAG}: clean: {scan.prefixed} prefixed, {sum(allowed_counts.values())} allowed pending rename, "
        f"{scan.exempt} in a control schema, {len(scan.unchecked)} unchecked, of {scan.checked} object "
        f"name(s) in {scan.files} file(s) under {BACKENDS.as_posix()}/*/src/",
        file=stream,
    )
    return 0


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="Object-name prefix gate for backend crates (WP-CON-05.T4)")
    parser.add_argument("workspace", nargs="?", type=Path, default=HERE.parents[2])
    parser.add_argument("--allow", type=Path, default=HERE / "allow", help="allow-list directory (default: tools/lints/table-prefix/allow)")
    args = parser.parse_args(argv)
    return run(args.workspace.resolve(), args.allow)


if __name__ == "__main__":
    raise SystemExit(main())
