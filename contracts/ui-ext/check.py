#!/usr/bin/env python3
"""Contract checks for the UI extension contract, tier A (WP-UI-13.T1, ADR-069 amendment A1).

Run with the pinned requirements in ``requirements.txt``. This file is the *reference
validator*: the Rust validator (WP-UI-13.T2) and the Avalonia renderer (WP-UI-13.T3) must give the
same verdict, with the same ``uiext.*`` code, on every file of the shared corpus in ``examples/``.

Rules checked here:

1. ``VERSION`` is semver, ``CHANGELOG.md`` has a heading for it, and every schema ``$id`` carries it.
2. ``slot.json`` ``x-slots`` and the slot enum list the same slots; every slot accepts one kind.
3. Every ``examples/valid`` file and ``examples/sample-app/ui.json`` validates against
   ``schema/bundle.json`` and passes the semantic rules below with no finding.
4. Every ``examples/invalid`` file is refused, its ``$comment`` header ``expect: <code>`` names a code
   listed in ``errors.md``, and that code is among the findings. Every code in ``errors.md`` has at
   least one invalid example.
5. Expression strings are parsed with a complete implementation of the grammar of
   ``docs/expression-language.md`` section 3 (including the function names and their arity), so a
   string that parses here parses in ``spot-expr``. Evaluation is not performed here.

Semantic rules (beyond JSON Schema; each is one ``uiext.*`` code in ``errors.md``): namespacing and
origin, duplicate ids, slot kind, label length, slot limits, DocType required, permission exists,
method declared, destination declared, route declared, query declared and of the right kind,
referenced panel / component / action exists, no component cycle, depth at most 8, at most 500
nodes per expansion.
"""

from __future__ import annotations

import json
import re
import sys
from pathlib import Path
from typing import Any, Iterator

from jsonschema import Draft202012Validator
from referencing import Registry, Resource
from referencing.jsonschema import DRAFT202012

HERE = Path(__file__).resolve().parent
SCHEMA_DIR = HERE / "schema"
EXAMPLES = HERE / "examples"
CONTEXT_PATH = EXAMPLES / "sample-app" / "context.json"
SEMVER = re.compile(r"^\d+\.\d+\.\d+$")
CODE_RE = re.compile(r"`(uiext\.[a-z_]+)`")
MAX_DEPTH = 8
MAX_NODES = 500

Finding = tuple[str, str]  # (code, JSON pointer)


# ------------------------------------------------------------------------------------------
# Expression syntax (docs/expression-language.md section 3 and 8)
# ------------------------------------------------------------------------------------------

KEYWORDS = {"and", "or", "not", "in", "true", "false", "null"}
ARITY: dict[str, tuple[int, int | None]] = {
    "len": (1, 1), "lower": (1, 1), "upper": (1, 1), "trim": (1, 1), "contains": (2, 2),
    "starts_with": (2, 2), "ends_with": (2, 2), "substr": (3, 3), "replace": (3, 3),
    "is_empty": (1, 1), "coalesce": (1, None), "round": (2, 2), "abs": (1, 1), "min": (2, None),
    "max": (2, None), "int": (1, 1), "decimal": (1, 1), "text": (1, 1), "date": (1, 1),
    "datetime": (1, 1), "now": (0, 0), "date_add": (2, 2), "days_between": (2, 2),
    "year": (1, 1), "month": (1, 1), "day": (1, 1),
}
TOKEN = re.compile(
    r"""\s*(?:(?P<dec>\d+\.\d+)|(?P<int>\d+)|(?P<str>'(?:\\.|[^'\\])*'|"(?:\\.|[^"\\])*")|"""
    r"""(?P<id>[A-Za-z_][A-Za-z0-9_]*)|(?P<op>==|!=|<=|>=|[-+*/%<>()\[\],.?:]))""",
    re.S,
)


class ExprError(Exception):
    pass


def tokenize(src: str) -> list[tuple[str, str]]:
    out: list[tuple[str, str]] = []
    pos = 0
    while pos < len(src):
        if src[pos:].strip() == "":
            break
        m = TOKEN.match(src, pos)
        if not m:
            raise ExprError(f"unexpected character at {pos}")
        kind = m.lastgroup or ""
        out.append((kind, m.group(kind)))
        pos = m.end()
    return out


class Parser:
    def __init__(self, toks: list[tuple[str, str]]):
        self.t = toks
        self.i = 0

    def peek(self) -> tuple[str, str] | None:
        return self.t[self.i] if self.i < len(self.t) else None

    def take(self) -> tuple[str, str]:
        tok = self.peek()
        if tok is None:
            raise ExprError("unexpected end")
        self.i += 1
        return tok

    def is_op(self, v: str) -> bool:
        tok = self.peek()
        return tok is not None and tok[0] in ("op", "id") and tok[1] == v

    def expect(self, v: str) -> None:
        if not self.is_op(v):
            raise ExprError(f"expected {v!r}")
        self.i += 1

    def parse(self) -> None:
        self.ternary()
        if self.peek() is not None:
            raise ExprError("trailing input")

    def ternary(self) -> None:
        self.or_()
        if self.is_op("?"):
            self.i += 1
            self.ternary()
            self.expect(":")
            self.ternary()

    def or_(self) -> None:
        self.and_()
        while self.is_op("or"):
            self.i += 1
            self.and_()

    def and_(self) -> None:
        self.not_()
        while self.is_op("and"):
            self.i += 1
            self.not_()

    def not_(self) -> None:
        if self.is_op("not"):
            self.i += 1
            self.not_()
        else:
            self.comparison()

    def comparison(self) -> None:
        self.additive()
        for op in ("==", "!=", "<=", ">=", "<", ">", "in"):
            if self.is_op(op):
                self.i += 1
                self.additive()
                tok = self.peek()
                if tok is not None and tok[1] in ("==", "!=", "<=", ">=", "<", ">", "in"):
                    raise ExprError("comparison is not associative")
                return

    def additive(self) -> None:
        self.mult()
        while self.is_op("+") or self.is_op("-"):
            self.i += 1
            self.mult()

    def mult(self) -> None:
        self.unary()
        while self.is_op("*") or self.is_op("/") or self.is_op("%"):
            self.i += 1
            self.unary()

    def unary(self) -> None:
        if self.is_op("-"):
            self.i += 1
            self.unary()
        else:
            self.primary()

    def primary(self) -> None:
        kind, val = self.take()
        if kind in ("dec", "int", "str"):
            return
        if kind == "op" and val == "(":
            self.ternary()
            self.expect(")")
            return
        if kind == "op" and val == "[":
            if not self.is_op("]"):
                self.ternary()
                while self.is_op(","):
                    self.i += 1
                    self.ternary()
            self.expect("]")
            return
        if kind == "id":
            if val in ("true", "false", "null"):
                return
            if val in KEYWORDS:
                raise ExprError(f"keyword {val!r} cannot start an operand")
            if self.is_op("("):
                self.i += 1
                n = 0
                if not self.is_op(")"):
                    self.ternary()
                    n = 1
                    while self.is_op(","):
                        self.i += 1
                        self.ternary()
                        n += 1
                self.expect(")")
                if val not in ARITY:
                    raise ExprError(f"unknown function {val!r}")
                lo, hi = ARITY[val]
                if n < lo or (hi is not None and n > hi):
                    raise ExprError(f"wrong number of arguments for {val!r}")
                return
            if val in ("doc", "parent"):
                n = 0
                while self.is_op("."):
                    self.i += 1
                    kind2, _ = self.take()
                    if kind2 != "id":
                        raise ExprError("expected a field name")
                    n += 1
                if n == 0:
                    raise ExprError(f"bare {val!r} is not a value")
                return
            raise ExprError(f"unknown name {val!r}")
        raise ExprError(f"unexpected token {val!r}")


def expr_ok(src: str) -> bool:
    try:
        Parser(tokenize(src)).parse()
        return True
    except ExprError:
        return False


# ------------------------------------------------------------------------------------------
# Schemas
# ------------------------------------------------------------------------------------------

def load_registry() -> tuple[Registry, dict[str, Any]]:
    reg = Registry()
    schemas: dict[str, Any] = {}
    for p in sorted(SCHEMA_DIR.glob("*.json")):
        doc = json.loads(p.read_text(encoding="utf-8"))
        schemas[p.name] = doc
        reg = reg.with_resource(doc["$id"], Resource.from_contents(doc, default_specification=DRAFT202012))
    return reg, schemas


def schema_code(err: Any) -> str:
    """The `uiext.*` code of a schema error: the `x-error` of the failing subschema, or of one
    inside its `context` (a oneOf), else the generic ``uiext.schema``."""
    schema = err.schema if isinstance(err.schema, dict) else {}
    if "x-error" in schema:
        return schema["x-error"]
    for sub in err.context or []:
        code = schema_code(sub)
        if code != "uiext.schema":
            return code
    return "uiext.schema"


def schema_findings(doc: Any, validator: Draft202012Validator) -> list[Finding]:
    out: list[Finding] = []
    for err in validator.iter_errors(doc):
        ptr = "/" + "/".join(str(p) for p in err.absolute_path)
        out.append((schema_code(err), ptr))
    return out


# ------------------------------------------------------------------------------------------
# Semantic rules
# ------------------------------------------------------------------------------------------

def walk_nodes(node: dict[str, Any]) -> Iterator[dict[str, Any]]:
    yield node
    kind = node.get("node")
    if kind in ("row", "column", "grid", "group"):
        for c in node.get("children", []):
            yield from walk_nodes(c)
    elif kind == "repeat":
        yield from walk_nodes(node["body"])
    elif kind == "when":
        yield from walk_nodes(node["then"])
    elif kind == "use":
        for c in node.get("fill", {}).values():
            yield from walk_nodes(c)


def semantic_findings(doc: dict[str, Any], ctx: dict[str, Any], slots: dict[str, Any]) -> list[Finding]:
    out: list[Finding] = []
    app = doc["app"]
    kinds = [("actions", "action"), ("panels", "panel"), ("tiles", "tile"), ("tabs", "tab")]
    items: list[tuple[str, str, dict[str, Any]]] = []
    for key, kind in kinds:
        for i, it in enumerate(doc.get(key, [])):
            items.append((f"/{key}/{i}", kind, it))
    comps = {c["id"]: (f"/components/{i}", c) for i, c in enumerate(doc.get("components", []))}
    panels = {p["id"] for p in doc.get("panels", [])}
    actions = {a["id"] for a in doc.get("actions", [])}

    def expr(src: str, ptr: str) -> None:
        if not expr_ok(src):
            out.append(("uiext.expression_invalid", ptr))

    def dest(d: str, ptr: str) -> None:
        if d not in ctx["destinations"]:
            out.append(("uiext.unknown_destination", ptr))

    def open_(o: dict[str, Any], ptr: str) -> None:
        dest(o["destination"], ptr + "/destination")
        for k, v in o.get("filters", {}).items():
            expr(v, f"{ptr}/filters/{k}")

    # namespacing, duplicates, ownership
    seen: dict[str, str] = {}
    for ptr, _, it in items + [(p, "component", c) for p, c in comps.values()]:
        if it["origin"] != f"app:{app}" or not it["id"].startswith(app + "."):
            out.append(("uiext.origin_mismatch", ptr))
        if it["id"] in seen:
            out.append(("uiext.duplicate_id", ptr))
        seen[it["id"]] = ptr

    counts: dict[tuple[str, str], int] = {}
    for ptr, kind, it in items:
        slot = it["slot"]
        spec = slots[slot]
        if spec["accepts"] != kind:
            out.append(("uiext.slot_kind_mismatch", ptr + "/slot"))
        label = it["label"]
        if not label.startswith("key:") and len(label) > spec["label_max"]:
            out.append(("uiext.label_too_long", ptr + "/label"))
        if slot.startswith(("doc.", "list.")) and "doctype" not in it:
            out.append(("uiext.doctype_required", ptr))
        key = (slot, it.get("doctype", ""))
        counts[key] = counts.get(key, 0) + 1
        if counts[key] == spec["max_per_app"] + 1:
            out.append(("uiext.slot_limit", ptr))
        if "permission" in it and it["permission"] not in ctx["permissions"]:
            out.append(("uiext.unknown_permission", ptr + "/permission"))
        if "visible_if" in it:
            expr(it["visible_if"], ptr + "/visible_if")

    for i, a in enumerate(doc.get("actions", [])):
        ptr = f"/actions/{i}"
        if "enabled_if" in a:
            expr(a["enabled_if"], ptr + "/enabled_if")
        if "call" in a:
            c = a["call"]
            if c["method"] not in ctx["methods"].get(c["doctype"], []):
                out.append(("uiext.unknown_method", ptr + "/call/method"))
            for k, v in c.get("args", {}).items():
                expr(v, f"{ptr}/call/args/{k}")
            then = c.get("then")
            if isinstance(then, dict):
                open_(then["open"], ptr + "/call/then/open")
        if "open" in a:
            open_(a["open"], ptr + "/open")
        if "navigate_url" in a:
            u = a["navigate_url"]
            if re.match(r"^[A-Za-z][A-Za-z0-9+.-]*:", u) or u.startswith("//") or not u.startswith("/"):
                out.append(("uiext.external_url", ptr + "/navigate_url"))
            elif not any(u == r or u.startswith(r + "/") for r in ctx["routes"]):
                out.append(("uiext.unknown_route", ptr + "/navigate_url"))

    for i, p in enumerate(doc.get("panels", [])):
        for j, b in enumerate(p["blocks"]):
            ptr = f"/panels/{i}/blocks/{j}"
            t = b["type"]
            if t in ("table", "stat"):
                q = b.get("query")
                info = ctx["queries"].get(q)
                if info is None:
                    out.append(("uiext.unknown_query", ptr + "/query"))
                elif info["kind"] != ("rows" if t == "table" else "aggregate"):
                    out.append(("uiext.query_kind", ptr + "/query"))
            elif t == "key_value":
                for k, it in enumerate(b["items"]):
                    expr(it["value"], f"{ptr}/items/{k}/value")
            elif t == "list_of_links":
                for k, it in enumerate(b["items"]):
                    open_(it["open"], f"{ptr}/items/{k}/open")
            elif t == "text":
                for d in re.findall(r"\]\(dest:([^)]*)\)", b["markdown"]):
                    dest(d, ptr + "/markdown")

    for i, t in enumerate(doc.get("tabs", [])):
        if t["panel"] not in panels:
            out.append(("uiext.unknown_panel", f"/tabs/{i}/panel"))
    for i, t in enumerate(doc.get("tiles", [])):
        ptr = f"/tiles/{i}"
        if t["component"] not in comps:
            out.append(("uiext.unknown_component", ptr + "/component"))
        for k, v in t.get("props", {}).items():
            if isinstance(v, dict):
                info = ctx["queries"].get(v["query"])
                if info is None or info["kind"] != "aggregate":
                    out.append(("uiext.unknown_query", f"{ptr}/props/{k}"))
        if "open" in t:
            open_(t["open"], ptr + "/open")

    # components: expressions, references, cycles, limits
    graph: dict[str, set[str]] = {}
    for cid, (ptr, c) in comps.items():
        graph[cid] = set()
        for n in walk_nodes(c["body"]):
            kind = n["node"]
            if kind == "when":
                expr(n["test"], ptr + "/body")
            elif kind == "control":
                if "value" in n:
                    expr(n["value"], ptr + "/body")
                if "on_click" in n and n["on_click"] not in actions:
                    out.append(("uiext.unknown_action", ptr + "/body"))
            elif kind == "use":
                if n["component"] not in comps:
                    out.append(("uiext.unknown_component", ptr + "/body"))
                else:
                    graph[cid].add(n["component"])
                for v in n.get("props", {}).values():
                    expr(v, ptr + "/body")

    state: dict[str, int] = {}
    cyclic: set[str] = set()

    def dfs(n: str) -> None:
        state[n] = 1
        for m in graph.get(n, ()):
            if state.get(m) == 1:
                cyclic.add(n)
                cyclic.add(m)
            elif m not in state:
                dfs(m)
        state[n] = 2

    for cid in graph:
        if cid not in state:
            dfs(cid)
    for cid in sorted(cyclic):
        out.append(("uiext.component_cycle", comps[cid][0]))

    def measure(node: dict[str, Any]) -> tuple[int, int]:
        """(depth, nodes) of the expansion of a node."""
        k = node["node"]
        if k in ("row", "column", "grid", "group"):
            parts = [measure(c) for c in node.get("children", [])]
            return 1 + max((d for d, _ in parts), default=0), 1 + sum(n for _, n in parts)
        if k == "repeat":
            d, n = measure(node["body"])
            return 1 + d, 1 + node["max"] * n
        if k == "when":
            d, n = measure(node["then"])
            return 1 + d, 1 + n
        if k == "use":
            d, n = measure(comps[node["component"]][1]["body"])
            fills = [measure(c) for c in node.get("fill", {}).values()]
            return 1 + max([d] + [x for x, _ in fills]), 1 + n + sum(m for _, m in fills)
        return 1, 1

    if not cyclic and not any(c == "uiext.unknown_component" for c, _ in out):
        for cid, (ptr, c) in comps.items():
            depth, nodes = measure(c["body"])
            if depth > MAX_DEPTH:
                out.append(("uiext.component_too_deep", ptr))
            if nodes > MAX_NODES:
                out.append(("uiext.component_too_large", ptr))
    return out


# ------------------------------------------------------------------------------------------
# Driver
# ------------------------------------------------------------------------------------------

def main() -> int:
    errors: list[str] = []
    version = (HERE / "VERSION").read_text().strip()
    if not SEMVER.match(version):
        errors.append(f"rule 1: VERSION {version!r} is not semver")
    if f"## {version}" not in (HERE / "CHANGELOG.md").read_text():
        errors.append(f"rule 1: CHANGELOG.md has no `## {version}` heading")

    registry, schemas = load_registry()
    for name, s in schemas.items():
        if f"/{version}/" not in s["$id"]:
            errors.append(f"rule 1: {name} $id does not carry version {version}")
    slot_doc = schemas["slot.json"]
    slots = slot_doc["x-slots"]
    if set(slots) != set(slot_doc["$defs"]["slot"]["enum"]):
        errors.append("rule 2: slot.json x-slots and the slot enum differ")
    for s, spec in slots.items():
        if spec["accepts"] not in ("action", "panel", "tile", "tab"):
            errors.append(f"rule 2: slot {s} accepts {spec['accepts']!r}")

    validator = Draft202012Validator(schemas["bundle.json"], registry=registry)
    ctx = json.loads(CONTEXT_PATH.read_text())
    errors_md = (HERE / "errors.md").read_text()
    documented = set(CODE_RE.findall(errors_md))

    def findings(path: Path) -> list[Finding]:
        doc = json.loads(path.read_text(encoding="utf-8"))
        found = schema_findings(doc, validator)
        if not found:
            found = semantic_findings(doc, ctx, slots)
        return found

    good = sorted((EXAMPLES / "valid").glob("*.json")) + [EXAMPLES / "sample-app" / "ui.json"]
    for p in good:
        for code, ptr in findings(p):
            errors.append(f"rule 3: {p.relative_to(HERE)}: valid example refused: {code} at {ptr}")

    used: set[str] = set()
    bad = sorted((EXAMPLES / "invalid").glob("*.json"))
    if not bad:
        errors.append("rule 4: no invalid examples")
    for p in bad:
        doc = json.loads(p.read_text(encoding="utf-8"))
        m = re.match(r"expect: (uiext\.[a-z_]+)\b", doc.get("$comment", ""))
        if not m:
            errors.append(f"rule 4: {p.relative_to(HERE)}: header `$comment` must start `expect: <code>`")
            continue
        want = m.group(1)
        used.add(want)
        if want not in documented:
            errors.append(f"rule 4: {p.relative_to(HERE)}: {want} is not in errors.md")
        got = {c for c, _ in findings(p)}
        if want not in got:
            errors.append(f"rule 4: {p.relative_to(HERE)}: expected {want}, got {sorted(got) or 'no finding'}")
    for code in sorted(documented - used):
        errors.append(f"rule 4: {code} is in errors.md but no invalid example breaks it")

    if errors:
        print("ui-ext contract check FAILED:")
        for e in errors:
            print("  -", e)
        return 1
    print(f"ui-ext contract {version}: {len(good)} valid, {len(bad)} invalid examples, "
          f"{len(documented)} codes, all rules pass")
    return 0


if __name__ == "__main__":
    sys.exit(main())
