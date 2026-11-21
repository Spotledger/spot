"""A small WIT reader for the connector page (WP-DX-04.T2).

It is not a full WIT parser. It reads the constructs the platform's worlds use (a package, worlds
and interfaces holding enums, records, variants, flags, type aliases, `import` and `export` of
functions, and `use`) with their `///` doc comments, and it **refuses what it cannot read**: a
construct the reader does not know is an error naming the line, never silently skipped, so a
contract change cannot slip through the outline.
"""
from __future__ import annotations

import re
from dataclasses import dataclass, field


class WitError(Exception):
    """A construct the reader cannot read; the message names the line."""


@dataclass
class Item:
    kind: str  # enum, record, variant, flags, type, func, use, import, export
    name: str
    doc: str
    line: int
    detail: str = ""  # a signature or a type
    members: list[tuple[str, str, str]] = field(default_factory=list)  # (name, type, doc)
    direction: str = ""  # import / export for functions


@dataclass
class Scope:
    kind: str  # world or interface
    name: str
    doc: str
    line: int
    items: list[Item] = field(default_factory=list)


@dataclass
class Wit:
    package: str
    scopes: list[Scope]


IDENT = r"%?[a-z][a-z0-9]*(?:-[a-z0-9]+)*"
FUNC_RE = re.compile(
    rf"^(?:(import|export)\s+)?({IDENT})\s*:\s*func\s*\((.*)\)\s*(?:->\s*(.+?))?\s*;$", re.S
)
FIELD_RE = re.compile(rf"^({IDENT})\s*:\s*(.+?)\s*,?$", re.S)


def _strip(text: str) -> list[tuple[int, str, str]]:
    """Lines as (line number, code, pending doc comment). Plain `//` comments are dropped;
    `///` lines are collected and attached to the next code line."""
    lines: list[tuple[int, str, str]] = []
    doc: list[str] = []
    for number, raw in enumerate(text.splitlines(), start=1):
        stripped = raw.strip()
        if stripped.startswith("///"):
            doc.append(stripped[3:].strip())
            continue
        code = re.sub(r"//.*$", "", stripped).strip()
        if not code:
            if not stripped:
                doc = []
            continue
        lines.append((number, code, " ".join(doc)))
        doc = []
    return lines


def parse(text: str) -> Wit:
    lines = _strip(text)
    package = ""
    scopes: list[Scope] = []
    scope: Scope | None = None
    block: Item | None = None
    index = 0
    while index < len(lines):
        number, code, doc = lines[index]
        index += 1
        if block is not None:
            if code == "}":
                scope.items.append(block)  # type: ignore[union-attr]
                block = None
                continue
            if block.kind == "enum" or block.kind == "flags":
                match = re.fullmatch(rf"({IDENT})\s*,?", code)
                if not match:
                    raise WitError(f"line {number}: cannot read {block.kind} case: {code!r}")
                block.members.append((match.group(1), "", doc))
            elif block.kind == "record":
                match = FIELD_RE.match(code)
                if not match:
                    raise WitError(f"line {number}: cannot read record field: {code!r}")
                block.members.append((match.group(1), match.group(2).rstrip(","), doc))
            else:  # variant
                match = re.fullmatch(rf"({IDENT})(?:\((.+)\))?\s*,?", code)
                if not match:
                    raise WitError(f"line {number}: cannot read variant case: {code!r}")
                block.members.append((match.group(1), match.group(2) or "", doc))
            continue
        if scope is None:
            match = re.fullmatch(r"package\s+(\S+?)\s*;", code)
            if match:
                package = match.group(1)
                continue
            match = re.fullmatch(rf"(world|interface)\s+({IDENT})\s*\{{", code)
            if match:
                scope = Scope(match.group(1), match.group(2), doc, number)
                continue
            raise WitError(f"line {number}: cannot read top-level construct: {code!r}")
        if code == "}":
            scopes.append(scope)
            scope = None
            continue
        match = re.fullmatch(rf"(enum|record|variant|flags)\s+({IDENT})\s*\{{", code)
        if match:
            block = Item(match.group(1), match.group(2), doc, number)
            continue
        match = re.fullmatch(rf"type\s+({IDENT})\s*=\s*(.+?)\s*;", code)
        if match:
            scope.items.append(Item("type", match.group(1), doc, number, match.group(2)))
            continue
        match = re.fullmatch(r"use\s+(.+?)\s*;", code)
        if match:
            scope.items.append(Item("use", match.group(1), doc, number))
            continue
        # A function may span lines: gather until the closing semicolon.
        joined = code
        start = number
        while FUNC_RE.match(joined) is None and not joined.endswith(";") and index < len(lines):
            joined += " " + lines[index][1]
            index += 1
        match = FUNC_RE.match(joined)
        if match:
            direction, name, params, result = match.groups()
            signature = f"func({params.strip()})" + (f" -> {result.strip()}" if result else "")
            scope.items.append(Item("func", name, doc, start, signature, direction=direction or ""))
            continue
        match = re.fullmatch(rf"(import|export)\s+(\S+?)\s*;", joined)
        if match:
            scope.items.append(Item(match.group(1), match.group(2), doc, start))
            continue
        raise WitError(f"line {start}: cannot read construct: {joined!r}")
    if block is not None or scope is not None:
        raise WitError("the file ends inside an unclosed block")
    if not package:
        raise WitError("no `package` declaration")
    return Wit(package, scopes)
