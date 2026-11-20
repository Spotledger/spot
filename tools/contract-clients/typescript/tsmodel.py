"""Read the contract's operations into a small model for the emitters."""

from __future__ import annotations

from dataclasses import dataclass, field
from typing import Any

from tsescape import RESERVED_METHOD_NAMES, is_identifier
from tsreport import Report
from tsschema import Translator, resolve_pointer

HTTP_METHODS = ("get", "put", "post", "delete", "patch", "head", "options", "trace")
JSON_FIRST = ("application/json",)
TEXT_TYPES = ("application/x-ndjson", "application/xml", "application/samlmetadata+xml", "text/event-stream")


def is_json(content_type: str) -> bool:
    return content_type == "application/json" or content_type.endswith("+json")


def is_text(content_type: str) -> bool:
    return content_type.startswith("text/") or content_type in TEXT_TYPES


@dataclass
class Param:
    name: str
    where: str
    required: bool
    expr: str
    array: bool = False


@dataclass
class Body:
    content_type: str
    expr: str
    label: str
    required: bool


@dataclass
class Resp:
    status: str  # "200" ... or "default"
    content_type: str | None
    expr: str | None
    label: str | None
    headers: list[str] = field(default_factory=list)


@dataclass
class Operation:
    id: str
    method: str
    path: str
    summary: str
    description: str
    params: list[Param]
    request: Body | None
    responses: list[Resp]
    idempotent: bool
    etag: bool


def deref(root: dict[str, Any], node: Any, report: Report, where: str) -> Any:
    seen = 0
    while isinstance(node, dict) and "$ref" in node:
        try:
            node = resolve_pointer(root, node["$ref"])
        except (KeyError, IndexError, ValueError):
            report.finding(f"unresolved $ref {node['$ref']!r} at {where}")
            return {}
        seen += 1
        if seen > 20:
            report.finding(f"$ref chain too long at {where}")
            return {}
    return node


def media_choice(content: dict[str, Any], where: str, report: Report, role: str) -> str | None:
    """Pick one media type of a request or response; report what is not generated."""
    if not content:
        return None
    names = list(content)
    order = [n for n in names if is_json(n)] + [n for n in names if not is_json(n) and n == "application/octet-stream"]
    order += [n for n in names if n not in order]
    chosen = order[0]
    for other in names:
        if other != chosen:
            report.drop(where, f"{role} media type {other} (generated: {chosen})")
    return chosen


def body_kind(content_type: str) -> str:
    if is_json(content_type):
        return "json"
    if is_text(content_type):
        return "text"
    return "bytes"


def collect(root: dict[str, Any], report: Report) -> list[Operation]:
    typer = Translator(root, report, prefix="T.")
    label_typer = Translator(root, Report(), prefix="")
    ops: list[Operation] = []
    for path, item in (root.get("paths") or {}).items():
        shared = item.get("parameters") or []
        for method in HTTP_METHODS:
            if method not in item:
                continue
            op = item[method]
            oid = op.get("operationId")
            where = f"{method.upper()} {path}"
            if not isinstance(oid, str) or not is_identifier(oid) or oid in RESERVED_METHOD_NAMES:
                report.finding(f"{where}: operationId {oid!r} is missing or not a safe identifier")
                continue
            merged: dict[tuple[str, str], Any] = {}
            for raw in list(shared) + list(op.get("parameters") or []):
                param = deref(root, raw, report, where)
                merged[(param.get("name"), param.get("in"))] = param
            params: list[Param] = []
            for (name, loc), param in merged.items():
                if loc not in ("path", "query", "header"):
                    report.drop(f"{where} parameter {name}", f"location {loc!r} is not generated")
                    continue
                schema = param.get("schema", {})
                expr = typer.expr(schema, f"{oid}/parameters/{name}")
                is_array = isinstance(schema, dict) and schema.get("type") == "array"
                params.append(Param(name, loc, bool(param.get("required")) or loc == "path", expr, is_array))
            for name in _template_names(path):
                if not any(p.where == "path" and p.name == name for p in params):
                    report.finding(f"{where}: path template names {{{name}}} without a path parameter")
            request = None
            body = deref(root, op.get("requestBody"), report, where) if op.get("requestBody") else None
            if body:
                ctype = media_choice(body.get("content") or {}, where, report, "request")
                if ctype is not None:
                    kind = body_kind(ctype)
                    schema = body["content"][ctype].get("schema", {})
                    if ctype == "multipart/form-data":
                        expr = label = "FormParts"
                    elif ctype == "application/octet-stream" or kind == "bytes":
                        expr = label = "Uint8Array"
                    elif kind == "text":
                        expr = label = "string"
                    else:
                        expr = typer.expr(schema, f"{oid}/requestBody", 6)
                        label = _label(label_typer, schema, f"{oid}/requestBody")
                    request = Body(ctype, expr, label, bool(body.get("required")))
            responses: list[Resp] = []
            etag = any(p.where == "header" and p.name.lower() == "if-match" for p in params)
            for status, raw in (op.get("responses") or {}).items():
                status = str(status)
                if status != "default" and not (status.isdigit() and len(status) == 3):
                    report.finding(f"{where}: response key {status!r} is not a status code or default")
                    continue
                resp = deref(root, raw, report, f"{where} {status}")
                heads = sorted((resp.get("headers") or {}).keys())
                if any(h.lower() == "etag" for h in heads):
                    etag = True
                ctype = media_choice(resp.get("content") or {}, f"{where} {status}", report, "response")
                expr = label = None
                if ctype is not None:
                    kind = body_kind(ctype)
                    schema = resp["content"][ctype].get("schema", {})
                    if kind == "json":
                        expr = typer.expr(schema, f"{oid}/responses/{status}", 6)
                        label = _label(label_typer, schema, f"{oid}/responses/{status}")
                    elif kind == "text":
                        expr = label = "string"
                    else:
                        expr = label = "Uint8Array"
                responses.append(Resp(status, ctype, expr, label, heads))
            idem = any(p.where == "header" and p.name.lower() == "idempotency-key" for p in params)
            ops.append(
                Operation(
                    oid, method.upper(), path, str(op.get("summary") or ""), str(op.get("description") or ""),
                    params, request, responses, idem, etag,
                )
            )
    ops.sort(key=lambda o: o.id)
    ids = [o.id for o in ops]
    for dup in sorted({i for i in ids if ids.count(i) > 1}):
        report.finding(f"operationId {dup!r} is used by more than one operation")
    return ops


def _template_names(path: str) -> list[str]:
    out, i = [], 0
    while True:
        start = path.find("{", i)
        if start < 0:
            return out
        end = path.find("}", start)
        if end < 0:
            return out
        out.append(path[start + 1:end])
        i = end + 1


def _label(typer: Translator, schema: Any, path: str) -> str:
    text = typer.expr(schema, path, 0)
    return text if "\n" not in text and len(text) <= 80 else "inline"
