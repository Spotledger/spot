#!/usr/bin/env python3
"""Error-code completeness (WP-DOC-08): every code the repo raises is catalogued.

Scans `crates/`, `contracts/` and `harness/` (Rust sources outside `tests/` dirs and
`tests.rs`/`main.rs`) for every stable dotted error code:

1. **Raised** — string literals passed to `AppError::new`, `AppError::field`, `with_field`,
   `Message::info` or `Message::warning`, and to the local wrapper helpers that take the code
   as a parameter named `code` (`fn internal(code: &str, msg)` and friends). Test-module
   raises count: contract suites run them.
2. **Defined** — every dotted-lower-case string literal outside comments and `#[cfg(test)]`
   spans: `pub const X: &str` vocabulary modules, `fn code()` / `fn api_code()` match arms,
   the native API registry table, wire `code:` fields.

It fails (exit 1, listing file and line) when any such code is missing from
`crates/framework/spot-errors/catalogue/*.toml`, and also when a catalogue entry names a code
nothing raises (`retired = true` exempts it). The crate's own validator (spot-errors
`validated.rs`) proves the entries' shape; this scan proves the coverage.
"""

from __future__ import annotations

import argparse
import re
import sys
import tomllib
from dataclasses import dataclass, field as dc_field
from pathlib import Path

CTOR_RE = re.compile(r"\b(AppError::new|AppError::field|with_field|Message::info|Message::warning)\s*\(")
CODE_RE = re.compile(r"^[a-z][a-z0-9_]*(\.[a-z][a-z0-9_]*)+$")
# A parameter list: the binding name is the first identifier of each comma-separated part.
FN_PARAMS_RE = re.compile(r"\bfn\s+([A-Za-z_][A-Za-z0-9_]*)\s*\(([^()]*)\)")
# `include_str!`/`include_bytes!` carry paths, never codes: their arguments are skipped.
PATH_MACRO_RE = re.compile(r"\b(?:include_str|include_bytes)!\s*\(")
# A dotted literal ending in one of these is a file name (`script.ts`), never a code.
FILE_SUFFIXES = (".toml", ".rs", ".ts")
# Dotted literals that are not codes. Only the "defined" vocabulary scan skips them; a literal passed to an error
# constructor is still a raised code. Each is matched exactly (never by prefix or suffix), so a real code with a
# similar name is still found. Reviewed one by one on `dev` (WP-DOC-08.T2 review): the bundle entry name, a JS
# property the app review scans for, a manifest field path; file names; host names; systemd unit names; and the
# deployment model's field paths, which name the field a `DeploymentError::invalid`/`check_text` refusal points at.
NON_CODE_LITERALS = frozenset(
    {
        # bundle entry, JS property, manifest field path
        "app.wasm",
        "document.cookie",
        "app.requires_host",
        # file names whose extension is not one of FILE_SUFFIXES
        "json.tmp",
        "site.toml.new",
        "apply_fatal.csv",
        "server.log",
        "server.err.log",
        "spot.conf",
        "postgresql.conf",
        "postmaster.pid",
        "initdb.pw",
        # systemd unit names in the Linux agent's test model
        "ssh.service",
        "sshd.service",
        "nginx.service",
        "crond.service",
        # host and site names used as test inputs
        "nowhere.invalid",
        "conformance.test",
        # field paths in spot-deployment-model validation refusals
        "backend.replica.node",
        "backend.replicas",
        "backend.version",
        "migrations.outcome",
        "migrations.run",
        "overrides.app",
        "overrides.by",
        "overrides.target",
        "platform.os",
        "platform.version",
        "policies.approval.destructive_approvers",
        "policies.approval.review_approvers",
        "sync.hub",
        "sync.policy_revision",
        "topology.node",
        "topology.node.roles",
        "topology.nodes",
        # a test double's mail-refusal code inside a contract suite's expected wire form
        "mail.refused",
        # tracing span names of the document pipeline (WP-PLT-15.T3)
        "doc.op",
        "doc.phase",
        "doc.step",
    }
)
SCAN_ROOTS = ("crates", "contracts", "harness")
CATALOGUE_DIR = Path("crates/framework/spot-errors/catalogue")


@dataclass(frozen=True)
class Hit:
    """One code occurrence: the code, the file, the line, and how it was found."""

    code: str
    file: str
    line: int
    kind: str


@dataclass
class FileScan:
    """What one source file defined (code vocabulary) and raised (constructor calls)."""

    defined: list[Hit] = dc_field(default_factory=list)
    raised: list[Hit] = dc_field(default_factory=list)


def _is_ident(ch: str) -> bool:
    return ch.isalnum() or ch == "_"


def literal_at(src: str, i: int) -> tuple[str, int] | None:
    """The Rust literal starting at `src[i]`: `(kind, end)` with `end` one past it, else None.

    The one place that knows Rust literal syntax; every walker (comment masking, brace and
    argument spans, argument splitting, string extraction) goes through it. `kind` is
    `"string"` (a normal or byte string, escapes honoured: `\\"` and `\\\\` never close it),
    `"raw"` (`r"..."`, `r#"..."#`, `br"..."`: no escapes, closed by the matching hashes) or
    `"char"` (`'"'`, `'\\''`, `'\\\\'`, `'\\u{..}'`). A lifetime (`'a`, `'static`) is not a
    literal, so it returns None and the walker moves on one character.
    """
    n = len(src)
    c = src[i]
    if c == "r" and i + 1 < n and src[i + 1] in ('"', "#"):
        before = src[i - 1] if i > 0 else " "
        if before == "b" and i > 1:
            before = src[i - 2]
        if not _is_ident(before) or (src[i - 1] == "b" and i == 1):
            h = i + 1
            while h < n and src[h] == "#":
                h += 1
            if h < n and src[h] == '"':
                closer = '"' + "#" * (h - (i + 1))
                end = src.find(closer, h + 1)
                return "raw", (n if end == -1 else end + len(closer))
        return None
    if c == '"':
        j = i + 1
        while j < n:
            if src[j] == "\\":
                j += 2
                continue
            if src[j] == '"':
                return "string", j + 1
            j += 1
        return "string", n
    if c == "'":
        if i + 1 >= n:
            return None
        if src[i + 1] == "\\":
            j = i + 3  # past the escaped character; `\u{..}` and `\x41` run on to the quote
            while j < n and src[j] != "'" and src[j] != "\n":
                j += 1
            return ("char", j + 1) if j < n and src[j] == "'" else None
        if i + 2 < n and src[i + 2] == "'" and src[i + 1] not in ("'", "\n"):
            return "char", i + 3
    return None


def string_literals(text: str):
    """Yields `(start, value)` for every normal string literal of `text` (code context).

    Char literals, lifetimes and raw strings are walked over, never yielded, so a `'"'` cannot
    flip the scan into string mode and a code written inside a raw string is not counted.
    """
    i = 0
    n = len(text)
    while i < n:
        lit = literal_at(text, i)
        if lit is None:
            i += 1
            continue
        kind, end = lit
        if kind == "string":
            closed = end <= n and end - 1 > i and text[end - 1] == '"'
            yield i, text[i + 1 : end - 1 if closed else end]
        i = end


def mask_comments(src: str) -> str:
    """Blanks comments out, preserving every other offset (so line numbers hold).

    Handles line comments, nestable block comments and every Rust literal (`literal_at`), so
    a `//` or `/*` inside a literal never blinds the scanner and a `'"'` never opens a string;
    raw strings are blanked, since nothing in one is code.
    """
    out = list(src)
    i = 0
    n = len(src)
    while i < n:
        c = src[i]
        if c == "/" and src.startswith("//", i):
            j = src.find("\n", i)
            end = j if j != -1 else n
            for k in range(i, end):
                out[k] = " "
            i = end
            continue
        if c == "/" and src.startswith("/*", i):
            depth = 0
            j = i
            while j < n:
                if src.startswith("/*", j):
                    depth += 1
                    j += 2
                elif src.startswith("*/", j):
                    depth -= 1
                    j += 2
                    if depth == 0:
                        break
                else:
                    j += 1
            for k in range(i, min(j, n)):
                if out[k] != "\n":
                    out[k] = " "
            i = j
            continue
        lit = literal_at(src, i)
        if lit is not None:
            if lit[0] == "raw":  # nothing written in a raw string is code, so none of it counts
                for k in range(i, lit[1]):
                    if out[k] != "\n":
                        out[k] = " "
            i = lit[1]
            continue
        i += 1
    return "".join(out)


def blank_path_macros(masked: str) -> str:
    """Blanks the argument spans of `include_str!`/`include_bytes!` (paths, never codes)."""
    out = list(masked)
    for m in PATH_MACRO_RE.finditer(masked):
        open_pos = m.end() - 1
        depth = 0
        i = open_pos
        n = len(masked)
        while i < n:
            if masked[i] == "(":
                depth += 1
            elif masked[i] == ")":
                depth -= 1
                if depth == 0:
                    for k in range(open_pos + 1, i):
                        if out[k] != "\n":
                            out[k] = " "
                    break
            i += 1
    return "".join(out)


def brace_span(src: str, open_pos: int) -> int:
    """The index one past the `}` closing the block opened at `open_pos`."""
    depth = 0
    i = open_pos
    n = len(src)
    while i < n:
        c = src[i]
        if c == "/" and src.startswith("//", i):
            i = src.find("\n", i)
            if i == -1:
                return n
            continue
        if c == "/" and src.startswith("/*", i):
            inner = 1
            i += 2
            while i < n and inner:
                if src.startswith("/*", i):
                    inner += 1
                    i += 2
                elif src.startswith("*/", i):
                    inner -= 1
                    i += 2
                else:
                    i += 1
            continue
        lit = literal_at(src, i)
        if lit is not None:
            i = lit[1]
            continue
        if c == "{":
            depth += 1
        elif c == "}":
            depth -= 1
            if depth == 0:
                return i + 1
        i += 1
    return n


def cfg_test_spans(src: str) -> list[tuple[int, int]]:
    """Spans of `#[cfg(test)]` gates: whole modules and single gated items.

    Both keep literals out of the shipped vocabulary, so a code defined only under one is
    neither required to be catalogued nor counted as raising it.
    """
    spans: list[tuple[int, int]] = []
    for m in re.finditer(r"#\[cfg\(test\)\]", src):
        after = re.compile(r"\b(mod|fn|const|static|impl|struct|enum)\b").search(src, m.end())
        if after is None:
            continue
        brace = src.find("{", after.end())
        if brace == -1 or after.group(1) in ("const", "static"):
            end = src.find("\n", after.end())
            spans.append((m.start(), end if end != -1 else len(src)))
            continue
        spans.append((m.start(), brace_span(src, brace)))
    return spans


def in_spans(pos: int, spans: list[tuple[int, int]]) -> bool:
    """Whether `pos` sits inside any span."""
    return any(start <= pos < end for start, end in spans)


def args_span(src: str, open_pos: int) -> int:
    """The index one past the `)` matching the `(` at `open_pos` (literals respected)."""
    depth = 0
    i = open_pos
    n = len(src)
    while i < n:
        c = src[i]
        lit = literal_at(src, i)
        if lit is not None:
            i = lit[1]
            continue
        if c == "(":
            depth += 1
        elif c == ")":
            depth -= 1
            if depth == 0:
                return i + 1
        i += 1
    return n


def split_args(args: str) -> list[str]:
    """Splits a call's argument text at top-level commas (nesting and literals respected)."""
    parts: list[str] = []
    depth = 0
    start = 0
    i = 0
    n = len(args)
    while i < n:
        lit = literal_at(args, i)
        if lit is not None:
            i = lit[1]
            continue
        ch = args[i]
        if ch in "(<[":
            depth += 1
        elif ch in ")]>":
            depth -= 1
        elif ch == "," and depth == 0:
            parts.append(args[start:i])
            start = i + 1
        i += 1
    if args[start:].strip():
        parts.append(args[start:])
    return parts


def wrapper_code_params(src: str) -> dict[str, int]:
    """`fn name(... code: &str ...)` helpers of one file: name -> 0-based code param ordinal.

    The repo raises through such wrappers (`fn internal(code: &str, msg: ...)`); their call
    sites count as raises, exactly like the constructors.
    """
    wrappers: dict[str, int] = {}
    for m in FN_PARAMS_RE.finditer(src):
        ordinal = 0
        for part in split_args(m.group(2)):
            text = part.strip()
            name = text.replace("mut ", "", 1).split(":")[0].strip()
            if name == "code" and ": &" in text:
                wrappers[m.group(1)] = ordinal
                break
            ordinal += 1
    return wrappers


def is_scanned(rel: str) -> bool:
    """Whether a repo-relative path is scanned (test trees and entrypoints are not)."""
    parts = Path(rel).parts
    if any(part == "tests" for part in parts[:-1]):
        return False
    return Path(rel).name not in ("tests.rs", "main.rs")


def scan_file(path: Path, rel: str) -> FileScan:
    """Extracts the defined vocabulary and the raised codes of one Rust file."""
    src = path.read_text(encoding="utf-8")
    masked = blank_path_macros(mask_comments(src))
    spans = cfg_test_spans(masked)
    wrappers = wrapper_code_params(masked)

    def line_of(text: str, pos: int) -> int:
        return text[:pos].count("\n") + 1

    scan = FileScan()

    # Defined: every dotted-lower-case literal outside comment and cfg(test) spans. File names
    # (`.toml`, `.rs`, `.ts`) and the few known non-code literals are never codes.
    for start, value in string_literals(masked):
        if (
            CODE_RE.match(value)
            and not value.endswith(FILE_SUFFIXES)
            and value not in NON_CODE_LITERALS
            and not in_spans(start, spans)
        ):
            scan.defined.append(Hit(value, rel, line_of(masked, start), "literal"))

    # Raised: dotted codes in constructor argument spans, outside cfg(test) gates (a
    # gated raise ships to no caller; contract suites that count are not gated).
    for m in CTOR_RE.finditer(masked):
        if in_spans(m.start(), spans):
            continue
        end = args_span(masked, m.end() - 1)
        for _, value in string_literals(masked[m.end() : end]):
            if CODE_RE.match(value):
                scan.raised.append(Hit(value, rel, line_of(masked, m.start()), "ctor:" + m.group(1)))

    # Raised through wrappers: the argument at the wrapper's code-param ordinal. Wrapper
    # calls under `#[cfg(test)]` are parsers and test scaffolding, not raises, so spans apply.
    for name, ordinal in wrappers.items():
        pattern = re.compile(rf"\b{name}\s*\(")
        for m in pattern.finditer(masked):
            if in_spans(m.start(), spans):
                continue
            end = args_span(masked, m.end() - 1)
            parts = split_args(masked[m.end() : end - 1])
            if ordinal >= len(parts):
                continue
            for _, value in string_literals(parts[ordinal]):
                if CODE_RE.match(value):
                    scan.raised.append(Hit(value, rel, line_of(masked, m.start()), f"wrapper:{name}"))
    return scan


def collect(root: Path) -> tuple[dict[str, list[Hit]], dict[str, list[Hit]]]:
    """Walks the scan roots; returns defined and raised codes, hits grouped by code."""
    defined: dict[str, list[Hit]] = {}
    raised: dict[str, list[Hit]] = {}
    for base in SCAN_ROOTS:
        top = root / base
        if not top.is_dir():
            continue
        for path in sorted(top.rglob("*.rs")):
            if "target" in path.parts:
                continue
            rel = path.relative_to(root).as_posix()
            if not is_scanned(rel):
                continue
            scan = scan_file(path, rel)
            for hit in scan.defined:
                defined.setdefault(hit.code, []).append(hit)
            for hit in scan.raised:
                raised.setdefault(hit.code, []).append(hit)
    return defined, raised


def catalogue_codes(root: Path) -> tuple[dict[str, tuple[str, bool]], int]:
    """The catalogue's codes: code -> (file name, retired); plus the TOML file count."""
    codes: dict[str, tuple[str, bool]] = {}
    cat_dir = root / CATALOGUE_DIR
    tomls = 0
    for path in sorted(cat_dir.glob("*.toml")):
        tomls += 1
        data = tomllib.loads(path.read_text(encoding="utf-8"))
        for i, entry in enumerate(data.get("entry", [])):
            code = entry.get("code", "")
            if not CODE_RE.match(code):
                raise ValueError(
                    f"{CATALOGUE_DIR / path.name}: entry {i + 1} code {code!r} is not dotted lower-case"
                )
            if code in codes:
                raise ValueError(f"{CATALOGUE_DIR / path.name}: entry {i + 1} duplicates code {code}")
            codes[code] = (path.name, bool(entry.get("retired", False)))
    return codes, tomls


def count_scanned(root: Path) -> int:
    """How many source files the scan walks (for the summary line)."""
    total = 0
    for base in SCAN_ROOTS:
        top = root / base
        if not top.is_dir():
            continue
        for path in top.rglob("*.rs"):
            if "target" in path.parts:
                continue
            if is_scanned(path.relative_to(root).as_posix()):
                total += 1
    return total


@dataclass
class Report:
    """The scan's findings, grouped to print one line per problem."""

    missing: list[Hit] = dc_field(default_factory=list)
    unraised: list[tuple[str, str]] = dc_field(default_factory=list)
    scanned_files: int = 0
    defined_codes: int = 0
    raised_codes: int = 0
    catalogued: int = 0
    catalogue_files: int = 0


def check(root: Path) -> Report:
    """Runs the completeness check; the report is the outcome, `run` turns it into an exit."""
    defined, raised = collect(root)
    catalogued, catalogue_files = catalogue_codes(root)
    everything = set(defined) | set(raised)
    missing: list[Hit] = []
    for code in sorted(everything):
        if code not in catalogued:
            hits = raised.get(code) or defined.get(code) or []
            missing.extend(hits[:3])
    unraised = [
        (code, file)
        for code, (file, retired) in sorted(catalogued.items())
        if not retired and code not in everything
    ]
    return Report(
        missing=missing,
        unraised=unraised,
        scanned_files=count_scanned(root),
        defined_codes=len(defined),
        raised_codes=len(raised),
        catalogued=len(catalogued),
        catalogue_files=catalogue_files,
    )


def run(root: Path) -> int:
    """The lint entry: 0 = complete, 1 = gap, 2 = environment trouble."""
    if not (root / "crates").is_dir():
        print("ERR-CODES: no crates/ directory, nothing to check")
        return 0
    if not (root / CATALOGUE_DIR).is_dir():
        print(
            f"ERR-CODES: {CATALOGUE_DIR} is missing; the catalogue is this repo's gate, not optional",
            file=sys.stderr,
        )
        return 2
    try:
        report = check(root)
    except (tomllib.TOMLDecodeError, ValueError) as exc:
        print(f"ERR-CODES: {exc}", file=sys.stderr)
        return 1
    for hit in report.missing:
        print(f"ERR-CODES: {hit.file}:{hit.line}: {hit.code} is raised/defined but not catalogued ({hit.kind})")
    for code, file in report.unraised:
        print(
            f"ERR-CODES: crates/framework/spot-errors/catalogue/{file}: "
            f"{code} is catalogued but no scanned source raises it"
        )
    if not report.missing and not report.unraised:
        print(
            f"ERR-CODES: every raised or defined code is catalogued and every entry is "
            f"raised or retired ({report.catalogued} codes in {report.catalogue_files} files, "
            f"{report.scanned_files} source files, {report.defined_codes} defined, "
            f"{report.raised_codes} raised)"
        )
        return 0
    return 1


def main() -> int:
    parser = argparse.ArgumentParser(description="Error-code completeness scan (WP-DOC-08).")
    parser.add_argument("workspace", nargs="?", type=Path, default=Path.cwd())
    args = parser.parse_args()
    return run(args.workspace.resolve())


if __name__ == "__main__":
    raise SystemExit(main())