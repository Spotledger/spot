"""Emit types.ts, operations.ts, index.ts and contract.ts."""

from __future__ import annotations

import json
from typing import Any

from tsescape import RESERVED_TYPE_NAMES, doc_comment, is_identifier, ts_key, ts_string
from tsmodel import Operation, Param, Resp
from tsreport import Report
from tsschema import Translator

TABLE_BEGIN = "// BEGIN OPERATIONS TABLE"
TABLE_END = "// END OPERATIONS TABLE"


def header(version: str, what: str) -> str:
    return (
        f"// {what}\n"
        f"// Generated from contracts/native-api/openapi.yaml, contract version {version}.\n"
        "// Generated, do not edit. Regenerate with tools/contract-clients/typescript/generate.py.\n\n"
    )


def emit_types(root: dict[str, Any], version: str, report: Report) -> str:
    typer = Translator(root, report, prefix="")
    out = [header(version, "types.ts: one exported type per schema of the contract (WP-WEB-10.T6).")]
    schemas = (root.get("components") or {}).get("schemas") or {}
    for name in sorted(schemas):
        if not is_identifier(name) or name in RESERVED_TYPE_NAMES:
            report.finding(f"schema name {name!r} is not usable as a TypeScript type name")
            continue
        schema = schemas[name]
        path = f"#/components/schemas/{name}"
        desc = schema.get("description") if isinstance(schema, dict) else None
        out.extend(line + "\n" for line in doc_comment(desc, "", 600))
        if typer.is_plain_object(schema):
            members = typer.members(schema, path, 2)
            out.append(f"export interface {name} {{\n" + "\n".join(members) + ("\n" if members else "") + "}\n\n")
        else:
            out.append(f"export type {name} = {typer.expr(schema, path, 0)};\n\n")
    return "".join(out)


def _param_json(p: Param) -> dict[str, Any]:
    out: dict[str, Any] = {"name": p.name, "required": p.required}
    if p.where == "query":
        out["array"] = p.array
    return out


def table_entry(op: Operation) -> dict[str, Any]:
    def where(loc: str) -> list[dict[str, Any]]:
        return [_param_json(p) for p in sorted(op.params, key=lambda q: q.name) if p.where == loc]

    success = [r for r in op.responses if r.status != "default" and int(r.status) < 400]
    errors = [r for r in op.responses if r.status == "default" or int(r.status) >= 400]
    return {
        "method": op.method,
        "path": op.path,
        "pathParams": where("path"),
        "queryParams": where("query"),
        "headerParams": where("header"),
        "request": None if op.request is None else {"contentType": op.request.content_type, "type": op.request.label},
        "success": [{"status": int(r.status), "contentType": r.content_type, "type": r.label} for r in success],
        "errors": {r.status: r.label for r in errors},
        "idempotent": op.idempotent,
        "etag": op.etag,
    }


def _arg_type(op: Operation) -> tuple[str, bool]:
    """The ``args`` type text and whether the argument may be omitted."""
    parts: list[str] = []
    required_any = False
    for loc, key in (("path", "path"), ("query", "query"), ("header", "headers")):
        group = sorted((p for p in op.params if p.where == loc), key=lambda q: q.name)
        if not group:
            continue
        need = any(p.required for p in group)
        required_any = required_any or need
        members = "; ".join(f"{ts_key(p.name)}{'' if p.required else '?'}: {p.expr}" for p in group)
        parts.append(f"{key}{'' if need else '?'}: {{ {members} }};")
    if op.request is not None:
        required_any = required_any or op.request.required
        parts.append(f"body{'' if op.request.required else '?'}: {op.request.expr};")
    if not parts:
        return "{ [key: string]: never }", True
    body = "\n".join(f"      {p}" for p in parts)
    return f"{{\n{body}\n    }}", not required_any


def _result_type(op: Operation) -> str:
    lines: list[str] = []
    default: Resp | None = None
    for r in sorted((r for r in op.responses if r.status != "default"), key=lambda r: int(r.status)):
        field_name = "body" if int(r.status) < 400 else "problem"
        value = r.expr if r.expr is not None else "null"
        lines.append(f"      | {{ status: {int(r.status)}; unexpected?: false; headers: ResponseHeaders; {field_name}: {value} }}")
    for r in op.responses:
        if r.status == "default":
            default = r
    value = default.expr if default is not None and default.expr is not None else "unknown"
    lines.append(f"      | {{ status: number; unexpected: true; headers: ResponseHeaders; problem: {value} }}")
    return "\n".join(lines).lstrip()


def emit_operations(ops: list[Operation], version: str) -> str:
    table = {op.id: table_entry(op) for op in ops}
    out = [header(version, "operations.ts: the operation table and the per-operation types (WP-WEB-10.T6).")]
    out.append('import type * as T from "./types";\n')
    out.append('import type { FormParts, ResponseHeaders } from "./client";\n\n')
    out.append("// The runtime table. `type` names the body type for orientation; the compiler\n")
    out.append("// checks calls against the `Operations` interface below.\n")
    out.append("const table = " + TABLE_BEGIN + "\n")
    out.append(json.dumps(table, indent=2, sort_keys=True, ensure_ascii=True) + "\n")
    out.append(TABLE_END + " as const;\n\n")
    out.append("export const operations = Object.freeze(table);\n\n")
    out.append("export type OperationId = keyof typeof operations;\n\n")
    out.append("export interface Operations {\n")
    for op in ops:
        args, _ = _arg_type(op)
        out.extend(f"{line}\n" for line in doc_comment(op.summary or op.description, "  ", 300))
        out.append(f"  {ts_key(op.id)}: {{\n    args: {args};\n    result:\n      {_result_type(op)};\n  }};\n")
    out.append("}\n")
    out.append("export type OperationArgs<K extends OperationId> = Operations[K][\"args\"];\n")
    out.append("export type OperationResult<K extends OperationId> = Operations[K][\"result\"];\n")
    return "".join(out)


def emit_index(version: str) -> str:
    return header(version, "index.ts: the public surface (WP-WEB-10.T6).") + (
        'export * from "./types";\n'
        'export * from "./operations";\n'
        'export * from "./client";\n'
        'export * from "./contract";\n'
    )


def emit_contract(version: str, sha256: str) -> str:
    return header(version, "contract.ts: the contract this surface was generated from (WP-WEB-10.T6).") + (
        f"export const CONTRACT_VERSION = {ts_string(version)};\n"
        f"export const CONTRACT_SHA256 = {ts_string(sha256)};\n"
    )
