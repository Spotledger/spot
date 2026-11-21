"""JSON Schema (OpenAPI 3.1) to TypeScript type text.

Plain text in, plain text out: no eval, no templates beyond string joins. Every
contract-provided string passes through tsescape.
"""

from __future__ import annotations

import json
from typing import Any

from tsescape import doc_comment, inline_comment, literal, ts_key
from tsreport import Report

MAX_SAFE = 2**53 - 1
# Annotation keywords that may sit next to a $ref without changing the type.
ANNOTATION_KEYS = {"description", "title", "default", "example", "examples", "deprecated", "readOnly", "writeOnly"}


def resolve_pointer(root: dict[str, Any], ref: str) -> Any:
    if not ref.startswith("#/"):
        raise KeyError(ref)
    node: Any = root
    for part in ref[2:].split("/"):
        token = part.replace("~1", "/").replace("~0", "~")
        node = node[int(token)] if isinstance(node, list) else node[token]
    return node


def wrap(expr: str) -> str:
    """Parenthesise a type that would bind wrongly next to ``[]`` or ``&``."""
    if " | " in expr or " & " in expr:
        return f"({expr})"
    return expr


class Translator:
    def __init__(self, root: dict[str, Any], report: Report, prefix: str = "") -> None:
        self.root = root
        self.report = report
        self.prefix = prefix
        self.schema_names = set((root.get("components") or {}).get("schemas") or {})

    # -- references --------------------------------------------------------
    def ref(self, ref: str, path: str) -> str:
        pre = "#/components/schemas/"
        if ref.startswith(pre):
            name = ref[len(pre):].replace("~1", "/").replace("~0", "~")
            if name in self.schema_names:
                return f"{self.prefix}{name}"
        self.report.finding(f"unresolved or non-schema $ref {ref!r} at {path}")
        return "unknown"

    # -- the entry point ---------------------------------------------------
    def expr(self, schema: Any, path: str, ind: int = 0) -> str:
        if schema is True or schema == {}:
            return "unknown"
        if schema is False:
            return "never"
        if not isinstance(schema, dict):
            self.report.finding(f"schema at {path} is neither an object nor a boolean")
            return "unknown"

        if "$ref" in schema:
            extra = sorted(k for k in schema if k != "$ref" and k not in ANNOTATION_KEYS and not k.startswith("x-"))
            if extra:
                self.report.approximate(path, f"keywords {extra} next to $ref ignored")
            out = self.ref(schema["$ref"], path)
            return f"{out} | null" if schema.get("nullable") is True else out

        pieces: list[str] = []
        base = self.base(schema, path, ind)
        if base is not None:
            pieces.append(base)
        for i, sub in enumerate(schema.get("allOf") or []):
            pieces.append(wrap(self.expr(sub, f"{path}/allOf/{i}", ind)))
        for key in ("oneOf", "anyOf"):
            if schema.get(key):
                alts = [self.expr(sub, f"{path}/{key}/{i}", ind) for i, sub in enumerate(schema[key])]
                if key == "anyOf":
                    self.report.approximate(path, "anyOf translated as a union")
                pieces.append(" | ".join(alts))
        if not pieces:
            out = "unknown"
        elif len(pieces) == 1:
            out = pieces[0]
        else:
            out = " & ".join(wrap(p) if " | " in p else p for p in pieces)
        if "not" in schema:
            note = json.dumps(schema["not"], sort_keys=True, default=str)
            self.report.approximate(path, "`not` has no TypeScript form; kept as a comment")
            out = f"{inline_comment('not: ' + note)} {out}"
        if schema.get("nullable") is True and "null" not in out.split(" | "):
            out = f"{wrap(out)} | null" if " & " in out else f"{out} | null"
        return out

    # -- the structural core ----------------------------------------------
    def base(self, schema: dict[str, Any], path: str, ind: int) -> str | None:
        if "const" in schema:
            lit = literal(schema["const"])
            if lit is None:
                self.report.approximate(path, "`const` of a non-scalar value became unknown")
                return "unknown"
            return lit
        if "enum" in schema:
            lits = []
            for value in schema["enum"]:
                lit = literal(value)
                if lit is None:
                    self.report.approximate(path, "`enum` member of a non-scalar value became unknown")
                    return "unknown"
                lits.append(lit)
            return " | ".join(lits) if lits else "never"
        spec = schema.get("type")
        if spec is None:
            if "properties" in schema or "additionalProperties" in schema:
                return self.object_expr(schema, path, ind)
            if "items" in schema:
                return self.array_expr(schema, path, ind)
            return None
        kinds = spec if isinstance(spec, list) else [spec]
        outs: list[str] = []
        for kind in kinds:
            out = self.one_type(kind, schema, path, ind)
            if out not in outs:
                outs.append(out)
        return " | ".join(outs)

    def one_type(self, kind: str, schema: dict[str, Any], path: str, ind: int) -> str:
        if kind == "null":
            return "null"
        if kind == "boolean":
            return "boolean"
        if kind == "string":
            if schema.get("format") == "binary":
                return "Uint8Array"
            return "string"
        if kind == "integer":
            self.check_integer(schema, path)
            return "number"
        if kind == "number":
            return "number"
        if kind == "array":
            return self.array_expr(schema, path, ind)
        if kind == "object":
            return self.object_expr(schema, path, ind)
        self.report.finding(f"unknown schema type {kind!r} at {path}")
        return "unknown"

    def check_integer(self, schema: dict[str, Any], path: str) -> None:
        """Money, quantities and ids never become a lossy JS number (ADR-12)."""
        if schema.get("format") in ("int64", "uint64"):
            self.report.refuse(f"{path} (format {schema['format']})")
            return
        for key in ("minimum", "maximum", "exclusiveMinimum", "exclusiveMaximum"):
            bound = schema.get(key)
            if isinstance(bound, (int, float)) and not isinstance(bound, bool) and abs(bound) > MAX_SAFE:
                self.report.refuse(f"{path} ({key} {bound} beyond 2^53)")

    def array_expr(self, schema: dict[str, Any], path: str, ind: int) -> str:
        if "prefixItems" in schema:
            self.report.approximate(path, "`prefixItems` ignored; array of the `items` type")
        if "items" not in schema:
            return "unknown[]"
        return f"{wrap(self.expr(schema['items'], path + '/items', ind))}[]"

    # -- objects -----------------------------------------------------------
    def members(self, schema: dict[str, Any], path: str, ind: int) -> list[str]:
        """Property lines (with doc comments), one entry per output line."""
        pad = " " * ind
        props = schema.get("properties") or {}
        required = set(schema.get("required") or [])
        for name in sorted(required - set(props)):
            self.report.approximate(path, f"required member {name!r} has no property schema")
        lines: list[str] = []
        for name, sub in props.items():
            lines += doc_comment(sub.get("description") if isinstance(sub, dict) else None, pad, 300)
            mark = "" if name in required else "?"
            lines.append(f"{pad}{ts_key(name)}{mark}: {self.expr(sub, f'{path}/properties/{name}', ind)};")
        return lines

    def index_type(self, schema: dict[str, Any], path: str, ind: int) -> str | None:
        extra = schema.get("additionalProperties")
        if isinstance(extra, dict):
            return self.expr(extra, f"{path}/additionalProperties", ind)
        if extra is True:
            return "unknown"
        return None

    def object_expr(self, schema: dict[str, Any], path: str, ind: int) -> str:
        if "propertyNames" in schema:
            note = json.dumps(schema["propertyNames"], sort_keys=True, default=str)
            self.report.approximate(path, "`propertyNames` has no TypeScript form; keys stay string")
            prefix = inline_comment("propertyNames: " + note) + " "
        else:
            prefix = ""
        props = schema.get("properties") or {}
        index = self.index_type(schema, path, ind + 2)
        closed = schema.get("additionalProperties") is False
        if not props:
            if index is not None:
                return f"{prefix}{{ [key: string]: {index} }}"
            if closed:
                return f"{prefix}{{ [key: string]: never }}"
            return f"{prefix}{{ [key: string]: unknown }}"
        pad = " " * ind
        body = "{\n" + "\n".join(self.members(schema, path, ind + 2)) + f"\n{pad}}}"
        if index is not None:
            body += f" & {{ [key: string]: {index} }}"
        return prefix + body

    def is_plain_object(self, schema: Any) -> bool:
        """True when the schema can be an ``interface`` (named object)."""
        return (
            isinstance(schema, dict)
            and "$ref" not in schema
            and schema.get("type") == "object"
            and bool(schema.get("properties"))
            and not any(k in schema for k in ("allOf", "oneOf", "anyOf", "not", "const", "enum", "nullable", "propertyNames"))
            and not isinstance(schema.get("additionalProperties"), dict)
            and schema.get("additionalProperties") is not True
        )
