#!/usr/bin/env python3
"""Lint: product code opens no outbound connection except through spot-egress (WP-SEC-09.T6, R-030).

`spot-egress` is the one outbound policy (private, loopback, link-local and metadata ranges
denied, DNS pinned, redirects re-validated). A direct socket, HTTP client, SMTP client or TLS
connector in product code walks round it. This scan reads every `*.rs` file under `crates/`,
`apps/` and `harness/` and every `Cargo.toml` there, and reports each use of an outbound
primitive, with file and line. A hit is silenced only by an entry in `allow.toml` (see README.md).

The scan is a heuristic over text, written to fail closed: comments and string literals are
blanked first, `#[cfg(test)]` modules are skipped, `target/`, `tests/` and `benches/` directories
are not scanned. An import of a primitive is itself reported, because later uses of the imported
name carry no forbidden path.

Usage: egress_lint.py REPO_ROOT
Exit status: 0 clean, 1 violations, 2 unreadable input.
"""

from __future__ import annotations

import bisect
import re
import sys
import tomllib
from dataclasses import dataclass
from pathlib import Path

CODE_DIRECT = "egress.direct_outbound"
CODE_DEP = "egress.forbidden_dependency"
CODE_STALE = "egress.stale_allow"
CODE_REASON = "egress.allow_without_reason"
CODE_INVALID = "egress.allow_invalid"
CODE_DUPLICATE = "egress.allow_duplicate_path"
CODE_GAPS = "egress.gaps_grew"

SCAN_ROOTS = ("crates", "apps", "harness")
SKIP_DIRS = {"target", ".git", "node_modules", "worktrees", "tests", "benches"}
KINDS = {"egress-implementation", "loopback-only", "authorised-by-guard", "peer-pinned", "harness", "gap"}

# Crates whose mere presence under [dependencies] is a finding (names normalised: `-` -> `_`).
FORBIDDEN_CRATES = {
    "reqwest",
    "ureq",
    "isahc",
    "surf",
    "curl",
    "lettre",
    "native_tls",
    "tokio_native_tls",
    "hyper_rustls",
}
# Use-tree paths (segments) that are findings, matched as a prefix of an imported path.
FORBIDDEN_IMPORT_PREFIXES = [
    ("tokio", "net", "TcpStream"),
    ("hyper", "client"),
    ("hyper_util", "client"),
    ("tokio_rustls", "TlsConnector"),
    ("tokio_rustls", "client", "TlsConnector"),
    ("rustls", "ClientConnection"),
    ("std", "net", "UdpSocket", "connect"),
    ("tokio", "net", "UdpSocket", "connect"),
] + [(c,) for c in sorted(FORBIDDEN_CRATES - {"lettre"})]

# `lettre` is both a message builder and an SMTP client. These sub-paths build or parse a message
# and cannot open a connection, so a file that uses only them is not a finding (the crate under
# `[dependencies]` still is). Everything else under `lettre` (`transport`, `Transport`,
# `AsyncTransport`, `SmtpTransport`, a bare `lettre`) is.
LETTRE_SAFE = {"message", "address", "Address", "Message"}

# Patterns over blanked code (outside `use` statements).
BODY_PATTERNS: list[tuple[str, re.Pattern[str]]] = [
    ("TcpStream::connect", re.compile(r"\bTcpStream\s*::\s*connect\b")),
    ("tokio::net::TcpStream", re.compile(r"\btokio\s*::\s*net\s*::\s*TcpStream\b")),
    ("UdpSocket::connect", re.compile(r"\bUdpSocket\s*::\s*connect\b")),
    ("hyper client", re.compile(r"\b(?:hyper_util|hyper)\s*::\s*client\b")),
    (
        "HTTP/SMTP/TLS client crate",
        re.compile(r"\b(?:" + "|".join(sorted(FORBIDDEN_CRATES - {"lettre"})) + r")\b"),
    ),
    (
        "lettre transport",
        re.compile(r"\blettre\b(?!\s*::\s*(?:" + "|".join(sorted(LETTRE_SAFE)) + r")\b)"),
    ),
    ("TlsConnector", re.compile(r"\bTlsConnector\b")),
    ("rustls::ClientConnection", re.compile(r"\bClientConnection\b")),
]
TO_SOCKET_ADDRS = re.compile(r"\bToSocketAddrs\b|\.to_socket_addrs\s*\(")
CONNECT_CALL = re.compile(r"\bconnect(?:_timeout)?\s*\(")
USE_STMT = re.compile(r"\b(?:pub(?:\s*\([^)]*\))?\s+)?use\b([^;]*);|\bextern\s+crate\b([^;]*);")
CFG_TEST = re.compile(r"#\[cfg\(\s*test\s*\)\]|#\[cfg\(\s*all\(\s*test\b[^\]]*\]")


@dataclass(frozen=True)
class Violation:
    path: str
    line: int
    code: str
    message: str

    def __str__(self) -> str:
        return f"{self.path}:{self.line}: {self.code}: {self.message}"


class InputError(Exception):
    pass


def blank(source: str) -> str:
    """Source with comments and string/char literals blanked (same length and newlines)."""
    out = list(source)
    i, n = 0, len(source)

    def wipe(a: int, b: int) -> None:
        for k in range(a, b):
            if out[k] != "\n":
                out[k] = " "

    while i < n:
        c = source[i]
        if source.startswith("//", i):
            j = source.find("\n", i)
            j = n if j < 0 else j
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
            wipe(i, j)
            i = j
        elif c in "rb" and re.match(r'b?r#*"', source[i : i + 40]) and not (i and (source[i - 1].isalnum() or source[i - 1] == "_")):
            hashes = len(re.match(r"b?r(#*)", source[i:]).group(1))
            prefix = 2 if c == "b" else 1
            end_marker = '"' + "#" * hashes
            j = source.find(end_marker, i + prefix + hashes + 1)
            j = n if j < 0 else j + len(end_marker)
            wipe(i, j)
            i = j
        elif c == '"':
            j = i + 1
            while j < n and source[j] != '"':
                j += 2 if source[j] == "\\" else 1
            j = min(j + 1, n)
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
    return "".join(out)


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
    for m in CFG_TEST.finditer(code):
        brace = code.find("{", m.end())
        semi = code.find(";", m.end())
        if semi >= 0 and (brace < 0 or semi < brace):
            spans.append((m.start(), semi + 1))  # `#[cfg(test)] use ...;`
        elif brace >= 0:
            spans.append((m.start(), match_brace(code, brace)))
    return spans


def blank_spans(code: str, spans: list[tuple[int, int]]) -> str:
    out = list(code)
    for a, b in spans:
        for k in range(a, b):
            if out[k] != "\n":
                out[k] = " "
    return "".join(out)


def flatten_use(tree: str) -> list[tuple[str, ...]]:
    """Expands a use tree (`a::{b, c::d as e}`) into full paths, as tuples of segments."""
    tokens = re.findall(r"::|[{},*]|[A-Za-z_][A-Za-z0-9_]*|\bas\b", tree)
    pos = 0
    paths: list[tuple[str, ...]] = []

    def parse(prefix: tuple[str, ...]) -> None:
        nonlocal pos
        segs = list(prefix)
        while pos < len(tokens):
            tok = tokens[pos]
            if tok == "::":
                pos += 1
            elif tok == "{":
                pos += 1
                while pos < len(tokens) and tokens[pos] != "}":
                    if tokens[pos] == ",":
                        pos += 1
                        continue
                    parse(tuple(segs))
                if pos < len(tokens):
                    pos += 1  # `}`
                return
            elif tok in {",", "}"}:
                break
            elif tok == "as":
                pos += 2  # the alias name does not change the path
            elif tok == "*":
                pos += 1
            else:
                segs.append(tok)
                pos += 1
        paths.append(tuple(segs))

    parse(())
    return paths


def import_hit(tree: str) -> str | None:
    for path in flatten_use(tree):
        segs = tuple(s for s in path if s not in {"crate", "self"})
        if segs[:1] == ("lettre",) and (len(segs) == 1 or segs[1] not in LETTRE_SAFE):
            return "lettre"
        for pre in FORBIDDEN_IMPORT_PREFIXES:
            if segs[: len(pre)] == pre:
                return "::".join(pre)
    return None


# A file with none of these words anywhere (comments and strings included) cannot hit; skipping
# it avoids the character-by-character blanking, which dominates the run time.
QUICK = re.compile(
    r"TcpStream|UdpSocket|hyper|reqwest|ureq|isahc|surf|curl|lettre|TlsConnector|native_tls|ClientConnection|ToSocketAddrs|to_socket_addrs"
)


def scan_source(source: str, rel: str) -> list[Violation]:
    if not QUICK.search(source):
        return []
    code = blank(source)
    code = blank_spans(code, cfg_test_spans(code))
    starts = [0] + [m.end() for m in re.finditer("\n", source)]

    def line_of(offset: int) -> int:
        return bisect.bisect_right(starts, offset)

    found: dict[tuple[int, str], Violation] = {}

    def add(line: int, what: str, message: str) -> None:
        found.setdefault((line, what), Violation(rel, line, CODE_DIRECT, message))

    use_spans = []
    for m in USE_STMT.finditer(code):
        use_spans.append((m.start(), m.end()))
        tree = m.group(1) if m.group(1) is not None else m.group(2)
        hit = import_hit(tree or "")
        if hit:
            add(line_of(m.start()), "import:" + hit, f"imports an outbound primitive ({hit}); use spot-egress")
    body = blank_spans(code, use_spans)
    for what, pattern in BODY_PATTERNS:
        for m in pattern.finditer(body):
            add(line_of(m.start()), what, f"opens an outbound connection directly ({what}); use spot-egress")
    if CONNECT_CALL.search(body):
        for m in TO_SOCKET_ADDRS.finditer(body):
            add(line_of(m.start()), "ToSocketAddrs", "resolves an address and connects without the egress guard (ToSocketAddrs); use spot-egress")
    for m in USE_STMT.finditer(code):
        tree = m.group(1) if m.group(1) is not None else m.group(2)
        if tree and re.search(r"\bToSocketAddrs\b", tree) and CONNECT_CALL.search(body):
            add(line_of(m.start()), "ToSocketAddrs-import", "imports ToSocketAddrs in a file that connects; use spot-egress")
    return sorted(found.values(), key=lambda v: (v.line, v.message))


def normalise(name: str) -> str:
    return name.replace("-", "_")


def scan_cargo(text: str, rel: str) -> list[Violation]:
    try:
        data = tomllib.loads(text)
    except tomllib.TOMLDecodeError as err:
        raise InputError(f"{rel}: not valid TOML: {err}") from err
    tables: list[dict] = []
    if isinstance(data.get("dependencies"), dict):
        tables.append(data["dependencies"])
    for target in (data.get("target") or {}).values():
        if isinstance(target, dict) and isinstance(target.get("dependencies"), dict):
            tables.append(target["dependencies"])
    out: list[Violation] = []
    for table in tables:
        for key, spec in table.items():
            names = {normalise(key)}
            if isinstance(spec, dict) and isinstance(spec.get("package"), str):
                names.add(normalise(spec["package"]))
            bad = sorted(names & FORBIDDEN_CRATES)
            if bad:
                line = 1
                for i, raw in enumerate(text.splitlines(), 1):
                    if re.match(r"\s*" + re.escape(key) + r"\s*(=|\.)", raw):
                        line = i
                        break
                out.append(
                    Violation(rel, line, CODE_DEP, f"depends on an HTTP, SMTP or TLS client crate ({bad[0]}); use spot-egress")
                )
    return out


def walk(root: Path):
    """Yields (path, relative posix path) for scannable `*.rs` and `Cargo.toml` files."""
    for top in SCAN_ROOTS:
        base = root / top
        if not base.is_dir():
            continue
        stack = [base]
        while stack:
            d = stack.pop()
            try:
                entries = sorted(d.iterdir())
            except OSError as err:
                raise InputError(f"cannot read directory {d}: {err}") from err
            for p in entries:
                if p.is_dir():
                    if p.name not in SKIP_DIRS:
                        stack.append(p)
                elif p.name == "Cargo.toml" or p.suffix == ".rs":
                    yield p, p.relative_to(root).as_posix()


def load_allow(root: Path) -> tuple[list[dict], int | None, list[Violation]]:
    path = root / "tools" / "lints" / "egress" / "allow.toml"
    rel = path.relative_to(root).as_posix()
    problems: list[Violation] = []
    try:
        data = tomllib.loads(path.read_text(encoding="utf-8"))
    except FileNotFoundError:
        return [], None, []
    except (OSError, tomllib.TOMLDecodeError) as err:
        raise InputError(f"{rel}: unreadable: {err}") from err
    max_gaps = data.get("max_gaps")
    if max_gaps is not None and (not isinstance(max_gaps, int) or isinstance(max_gaps, bool) or max_gaps < 0):
        problems.append(Violation(rel, 1, CODE_INVALID, "max_gaps must be a non-negative integer"))
        max_gaps = None
    entries = data.get("allow", [])
    if not isinstance(entries, list):
        raise InputError(f"{rel}: `allow` must be an array of tables")
    seen: set[str] = set()
    good: list[dict] = []
    for i, e in enumerate(entries, 1):
        where = f"entry {i}"
        if not isinstance(e, dict) or not isinstance(e.get("path"), str) or not e["path"]:
            problems.append(Violation(rel, 1, CODE_INVALID, f"{where}: missing `path`"))
            continue
        where = f"entry {e['path']}"
        reason = e.get("reason")
        if not isinstance(reason, str) or not reason.strip():
            problems.append(Violation(rel, 1, CODE_REASON, f"{where}: `reason` is empty"))
            continue
        if not isinstance(e.get("owner_wp"), str) or not e["owner_wp"].strip():
            problems.append(Violation(rel, 1, CODE_INVALID, f"{where}: missing `owner_wp`"))
            continue
        kind = e.get("kind")
        if kind not in KINDS:
            problems.append(Violation(rel, 1, CODE_INVALID, f"{where}: `kind` must be one of {sorted(KINDS)}"))
            continue
        needs = {"authorised-by-guard": "function", "peer-pinned": "pinned_at", "gap": "tracked_by"}.get(kind)
        if needs and not (isinstance(e.get(needs), str) and e[needs].strip()):
            problems.append(Violation(rel, 1, CODE_INVALID, f"{where}: kind `{kind}` needs a non-empty `{needs}`"))
            continue
        if e["path"] in seen:
            problems.append(Violation(rel, 1, CODE_DUPLICATE, f"{where}: more than one entry for this file"))
            continue
        seen.add(e["path"])
        good.append(e)
    return good, max_gaps, problems


def run(root: Path) -> tuple[int, list[str]]:
    if not root.is_dir():
        raise InputError(f"{root} is not a directory")
    hits: dict[str, list[Violation]] = {}
    for path, rel in walk(root):
        try:
            text = path.read_text(encoding="utf-8")
        except (OSError, UnicodeDecodeError) as err:
            raise InputError(f"cannot read {rel}: {err}") from err
        found = scan_cargo(text, rel) if path.name == "Cargo.toml" else scan_source(text, rel)
        if found:
            hits[rel] = found
    allow, max_gaps, problems = load_allow(root)
    allowed = {e["path"]: e for e in allow}
    violations: list[Violation] = list(problems)
    for rel, found in sorted(hits.items()):
        if rel not in allowed:
            violations.extend(found)
    allow_rel = "tools/lints/egress/allow.toml"
    for rel, e in sorted(allowed.items()):
        if rel not in hits:
            violations.append(Violation(allow_rel, 1, CODE_STALE, f"entry for {rel} matches no hit; remove it"))
    gaps = sum(1 for e in allow if e["kind"] == "gap")
    lines = [str(v) for v in sorted(violations, key=lambda v: (v.path, v.line, v.code))]
    summary = f"egress: gap entries: {gaps}" + (f" (max_gaps = {max_gaps})" if max_gaps is not None else " (no max_gaps line)")
    if max_gaps is not None and gaps > max_gaps:
        lines.append(f"{allow_rel}:1: {CODE_GAPS}: {gaps} gap entries exceed max_gaps = {max_gaps}; the list may only shrink")
    lines.append(summary)
    failed = any(not ln.startswith("egress: gap entries") for ln in lines)
    return (1 if failed else 0), lines


def main(argv: list[str]) -> int:
    if len(argv) != 2:
        print("usage: egress_lint.py REPO_ROOT", file=sys.stderr)
        return 2
    try:
        status, lines = run(Path(argv[1]))
    except InputError as err:
        print(f"egress: {err}", file=sys.stderr)
        return 2
    for line in lines:
        print(line)
    if status:
        print("egress: product code must reach the network through spot-egress; see tools/lints/egress/README.md", file=sys.stderr)
    return status


if __name__ == "__main__":
    sys.exit(main(sys.argv))
