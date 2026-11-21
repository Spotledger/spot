#!/usr/bin/env python3
"""Lint: no external effect performed directly in a controller (ADR-080 section 6; WP-EXT-11.T3).

External effects (mail, connector calls, webhooks, messages to other organisations) belong in
phase 4, announced through the outbox. A controller that calls an egress, mail, HTTP or socket API
itself would perform the effect inside the operation, where it can neither be held nor reversed
nor receipted. This scan reads the Rust sources of controllers (files containing
`impl Controller for`) and reports every use of an external-effect API inside them.

Two escapes exist, both visible in review:

* `// effect: committed-phase` on the lines just before a `fn` marks a function that performs
  external effects and may only run after commit. The scan accepts it only when the function is
  reachable from a `*_committed` step: every call of it, in the same file, sits in a function
  whose name ends in `_committed`, in a match arm for a step named `..._committed`, or in
  another function that is itself accepted the same way. A marked function that nothing in the
  file calls is reported too, since the scan cannot prove where it runs.
* `// effects-allow: <reason>` on the offending line or on a comment-only line above it, with a non-empty
  reason, for a false positive.

An import of an external-effect API (`use reqwest::Client;`, `use std::net::{TcpStream};`,
`use tokio::net as n;`) is itself reported, because later uses of the imported name carry no
forbidden path. Limits: only files that contain `impl Controller for` are scanned (a helper
module a controller calls into is not), and a committed-phase call is accepted only from a
function named `*_committed` or from the match arm whose pattern is made of `..._committed`
step names, never because such a literal sits nearby.

This is a heuristic over text, not a parser: it blanks comments and string literals first, skips
`#[cfg(test)]` modules, finds functions by brace matching, and resolves calls by
name within one file. It is written to fail closed on the cases it cannot see through.

Usage: check.py [PATH ...]   (default: crates apps under the repository root)
Exit status: 0 clean, 1 violations, 2 unreadable input or a named path that does not exist.
"""

from __future__ import annotations

import re
import sys
from dataclasses import dataclass
from pathlib import Path

CODE_DIRECT = "effects.direct_external_call"
CODE_COMMITTED = "effects.committed_phase_unreachable"
CODE_ALLOW_REASON = "effects.allow_without_reason"

CONTROLLER_IMPL = re.compile(r"\bimpl\b(?:\s*<[^{};]*?>)?\s+(?:[\w:]+::)?Controller\s+for\b")
FORBIDDEN = re.compile(
    r"\b(?:spot_egress|reqwest|spot_mail|lettre)\b|\b(?:std|tokio)\s*::\s*net\b|\bspot_webhooks\s*::"
)
# An import of an external-effect API is itself a finding in a controller file: once a name is
# imported (`use reqwest::Client;`, `use std::net::{TcpStream}`, `use tokio::net as n;`) its later
# uses carry no forbidden path any more, so the `use` statement is where it is caught.
USE_STMT = re.compile(r"\b(?:pub(?:\s*\([^)]*\))?\s+)?use\b[^;]*;|\bextern\s+crate\b[^;]*;")
USE_FORBIDDEN = re.compile(
    r"\b(?:spot_egress|reqwest|spot_mail|lettre|spot_webhooks)\b"
    r"|\b(?:std|tokio)\s*::\s*(?:\{[^;]*?\b)?net\b"
)
FN_DEF = re.compile(r"\bfn\s+([A-Za-z_]\w*)")
MARKER = re.compile(r"//\s*effect:\s*committed-phase\b")
ALLOW = re.compile(r"//\s*effects-allow:(.*)$")
SKIP_DIRS = {"target", ".git", "node_modules", "worktrees"}


@dataclass(frozen=True)
class Violation:
    path: str
    line: int
    code: str
    message: str

    def __str__(self) -> str:
        return f"{self.path}:{self.line}: {self.code}: {self.message}"


@dataclass
class Func:
    name: str
    start: int  # offset of `fn`
    open: int  # offset of `{`
    end: int  # offset just past the matching `}`
    marked: bool = False


def blank(source: str) -> tuple[str, dict[int, str], dict[int, list[str]], list[tuple[int, int, str]]]:
    """Returns the source with comments and string/char literals blanked (same length and
    newlines), the `//` comment text by 1-based line, the string literals by line, and the
    string literals as (start, end, text) offsets."""
    out = list(source)
    comments: dict[int, str] = {}
    strings: dict[int, list[str]] = {}
    spans: list[tuple[int, int, str]] = []
    i, n, line = 0, len(source), 1

    def wipe(a: int, b: int) -> None:
        for k in range(a, b):
            if out[k] != "\n":
                out[k] = " "

    while i < n:
        c = source[i]
        if c == "\n":
            line += 1
            i += 1
        elif source.startswith("//", i):
            j = source.find("\n", i)
            j = n if j < 0 else j
            comments[line] = source[i:j]
            wipe(i, j)
            i = j
        elif source.startswith("/*", i):
            depth, j = 1, i + 2
            while j < n and depth:
                if source.startswith("/*", j):
                    depth, j = depth + 1, j + 2
                elif source.startswith("*/", j):
                    depth, j = depth - 1, j + 2
                else:
                    j += 1
            line += source.count("\n", i, j)
            wipe(i, j)
            i = j
        elif c in "rb" and re.match(r'b?r#*"', source[i : i + 40]) and not (i and (source[i - 1].isalnum() or source[i - 1] == "_")):
            hashes = len(re.match(r"b?r(#*)", source[i:]).group(1))
            prefix = 2 if c == "b" else 1
            end_marker = '"' + "#" * hashes
            j = source.find(end_marker, i + prefix + hashes + 1)
            j = n if j < 0 else j + len(end_marker)
            strings.setdefault(line, []).append(source[i:j])
            spans.append((i, j, source[i:j]))
            line += source.count("\n", i, j)
            wipe(i, j)
            i = j
        elif c == '"':
            j = i + 1
            while j < n and source[j] != '"':
                j += 2 if source[j] == "\\" else 1
            j = min(j + 1, n)
            strings.setdefault(line, []).append(source[i:j])
            spans.append((i, j, source[i:j]))
            line += source.count("\n", i, j)
            wipe(i, j)
            i = j
        elif c == "'":
            m = re.match(r"'(?:\\.[^']*|[^\\'])'", source[i : i + 12])
            if m:
                wipe(i, i + m.end())
                i += m.end()
            else:
                i += 1  # a lifetime
        else:
            i += 1
    return "".join(out), comments, strings, spans


def match_brace(code: str, open_at: int) -> int:
    depth = 0
    for k in range(open_at, len(code)):
        if code[k] == "{":
            depth += 1
        elif code[k] == "}":
            depth -= 1
            if depth == 0:
                return k + 1
    return len(code)


def cfg_test_spans(code: str) -> list[tuple[int, int]]:
    spans = []
    for m in re.finditer(r"#\[cfg\(test\)\]", code):
        brace = code.find("{", m.end())
        semi = code.find(";", m.end())
        if brace >= 0 and (semi < 0 or brace < semi):
            spans.append((m.start(), match_brace(code, brace)))
    return spans


def functions(code: str, lines: list[str], line_of, comments: dict[int, str]) -> list[Func]:
    found: list[Func] = []
    for m in FN_DEF.finditer(code):
        depth, k = 0, m.end()
        while k < len(code):
            ch = code[k]
            if ch in "([<":
                depth += 1
            elif ch in ")]>" and not (ch == ">" and code[k - 1] == "-"):
                depth -= 1
            elif depth <= 0 and ch in "{;":
                break
            k += 1
        if k >= len(code) or code[k] == ";":
            continue  # a declaration without a body
        func = Func(m.group(1), m.start(), k, match_brace(code, k))
        func.marked = is_marked(lines, func.start, line_of, comments)
        found.append(func)
    return found


def is_marked(lines: list[str], start: int, line_of, comments: dict[int, str]) -> bool:
    """Whether the comment line(s) directly above a fn (attributes and modifiers allowed
    between) carry the committed-phase marker."""
    line = line_of(start)
    ln = line - 1
    while ln >= 1:
        text = comments.get(ln)
        raw_blank = lines[ln - 1].strip() if ln - 1 < len(lines) else ""
        if text is not None and MARKER.search(text):
            return True
        if raw_blank == "" and text is not None:
            ln -= 1
            continue
        if raw_blank.startswith("#["):
            ln -= 1
            continue
        break
    return False


def innermost(funcs: list[Func], pos: int) -> Func | None:
    best = None
    for f in funcs:
        if f.open <= pos < f.end and (best is None or f.open > best.open):
            best = f
    return best


def in_committed_arm(code: str, spans: list[tuple[int, int, str]], pos: int, floor: int) -> bool:
    """Whether `pos` sits inside the match arm whose pattern is made only of string literals
    ending in `_committed"`. Walks back from `pos` to the nearest `=>` at the same nesting
    level (leaving enclosing blocks and calls on the way, never past `floor`), then reads the
    pattern text between the previous arm boundary and that `=>`. A neighbouring arm never
    counts: only the arm the call is inside."""
    depth, k = 0, pos - 1
    while k >= floor:
        ch = code[k]
        if ch in ")]}":
            depth += 1
        elif ch in "([{":
            if depth > 0:
                depth -= 1
        elif ch == ">" and code[k - 1] == "=" and depth == 0:
            return arm_pattern_committed(code, spans, k - 1)
        k -= 1
    return False


def arm_pattern_committed(code: str, spans: list[tuple[int, int, str]], arrow: int) -> bool:
    depth, k = 0, arrow - 1
    while k >= 0:
        ch = code[k]
        if ch in ")]":
            depth += 1
        elif ch in "([":
            depth -= 1
        elif depth <= 0 and ch in ",{}":
            break
        k -= 1
    lits = [t for a, b, t in spans if k < a and b <= arrow]
    return bool(lits) and all(t.endswith('_committed"') for t in lits)


def scan_source(path: str, source: str) -> list[Violation]:
    code, comments, strings, spans = blank(source)
    if not CONTROLLER_IMPL.search(code):
        return []
    lines = code.split("\n")
    starts = [0] + [m.end() for m in re.finditer(r"\n", code)]

    def line_of(pos: int) -> int:
        lo, hi = 0, len(starts)
        while lo < hi - 1:
            mid = (lo + hi) // 2
            if starts[mid] <= pos:
                lo = mid
            else:
                hi = mid
        return lo + 1

    skip = cfg_test_spans(code)
    in_skip = lambda p: any(a <= p < b for a, b in skip)  # noqa: E731
    funcs = [f for f in functions(code, lines, line_of, comments) if not in_skip(f.start)]
    violations: list[Violation] = []

    source_lines = source.split("\n")

    def allowed(line: int) -> bool:
        # The line above counts only when it is a comment on its own; a trailing comment on a
        # code line excuses that line only.
        for ln in (line, line - 1):
            if ln != line and not (ln >= 1 and source_lines[ln - 1].lstrip().startswith("//")):
                continue
            m = ALLOW.search(comments.get(ln, ""))
            if m:
                if m.group(1).strip():
                    return True
                violations.append(
                    Violation(path, ln, CODE_ALLOW_REASON, "`// effects-allow:` needs a reason after the colon")
                )
        return False

    # Which marked functions are reachable only from committed steps (fixpoint).
    marked = {f.name: f for f in funcs if f.marked}
    ok: set[str] = set()
    callers: dict[str, list[tuple[int, Func | None]]] = {name: [] for name in marked}
    for name, f in marked.items():
        for m in re.finditer(r"(?<![\w])" + re.escape(name) + r"\s*(?:::<[^>]*>)?\s*\(", code):
            if in_skip(m.start()) or re.search(r"\bfn\s+$", code[max(0, m.start() - 12) : m.start()]):
                continue  # inside a test module, or the definition itself
            callers[name].append((m.start(), innermost(funcs, m.start())))

    def committed_site(pos: int, fn: Func | None) -> bool:
        if fn is not None and fn.name.endswith("_committed"):
            return True
        return in_committed_arm(code, spans, pos, fn.open if fn is not None else 0)

    changed = True
    while changed:
        changed = False
        for name, sites in callers.items():
            if name in ok or not sites:
                continue
            if all(
                committed_site(pos, fn) or (fn is not None and fn.name in ok and fn.name != name)
                for pos, fn in sites
            ):
                ok.add(name)
                changed = True
    for name, f in marked.items():
        if name not in ok:
            why = "nothing in this file calls it" if not callers[name] else "it is called from outside a `*_committed` step"
            violations.append(
                Violation(
                    path,
                    line_of(f.start),
                    CODE_COMMITTED,
                    f"`{name}` is marked `// effect: committed-phase` but {why}; "
                    "call it only from a `*_committed` step, or add `// effects-allow: <reason>`",
                )
            )

    # Forbidden API uses. `use` statements are matched whole, so a grouped or aliased import
    # (`use std::{net::TcpStream};`, `use tokio::net as n;`) is flagged where it is written.
    use_spans = [(m.start(), m.end()) for m in USE_STMT.finditer(code)]
    hits: list[tuple[int, str]] = []
    for a, b in use_spans:
        m = USE_FORBIDDEN.search(re.sub(r"\s+", " ", code[a:b]))
        if m:
            hits.append((a, re.sub(r"\s+", "", m.group(0)).split("{")[0] or m.group(0)))
    for m in FORBIDDEN.finditer(code):
        if not any(a <= m.start() < b for a, b in use_spans):
            hits.append((m.start(), re.sub(r"\s+", "", m.group(0))))
    for pos, what in sorted(hits):
        if in_skip(pos):
            continue
        line = line_of(pos)
        fn = innermost(funcs, pos)
        if fn is not None and fn.name in ok:
            continue
        if allowed(line):
            continue
        violations.append(
            Violation(
                path,
                line,
                CODE_DIRECT,
                f"`{what}` used directly in a controller; announce the effect in the committed "
                "phase through the outbox, or mark a committed-only function `// effect: committed-phase`",
            )
        )
    return sorted(set(violations), key=lambda v: (v.path, v.line, v.code))


def rust_files(roots: list[Path]):
    for root in roots:
        if root.is_file():
            yield root
            continue
        for p in sorted(root.rglob("*.rs")):
            # Judge the path below the root only: a checkout or worktree that itself lives under
            # a directory called `target` or `worktrees` must still be scanned.
            if not SKIP_DIRS.intersection(p.relative_to(root).parts):
                yield p


def run(roots: list[Path], base: Path | None = None) -> list[Violation]:
    found: list[Violation] = []
    for p in rust_files(roots):
        text = p.read_text(encoding="utf-8", errors="replace")
        shown = str(p.relative_to(base)) if base and p.is_relative_to(base) else str(p)
        found.extend(scan_source(shown.replace("\\", "/"), text))
    return found


def main(argv: list[str]) -> int:
    repo = Path(__file__).resolve().parents[3]
    explicit = [Path(a) for a in argv]
    missing = [str(r) for r in explicit if not r.exists()]
    if missing:
        print(f"effects lint: no such path: {', '.join(missing)}", file=sys.stderr)
        return 2
    roots = explicit or [r for r in (repo / "crates", repo / "apps") if r.exists()]
    try:
        violations = run(roots, repo)
    except OSError as e:
        print(f"effects lint: cannot read input: {e}", file=sys.stderr)
        return 2
    for v in violations:
        print(v)
    if violations:
        print(f"effects lint: {len(violations)} violation(s)", file=sys.stderr)
        return 1
    print("effects lint: clean")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
