#!/usr/bin/env python3
"""Convert the native API contract for the client generators (WP-CON-03.T2).

The contract ``contracts/native-api/openapi.yaml`` is OpenAPI 3.1 (ADR-055).

``to-30 <contract>``   Write an OpenAPI **3.0.3** JSON document to ``--output``.
                       The Rust generator (progenitor) only reads OpenAPI 3.0,
                       so the document is translated mechanically here, inside
                       this task, instead of changing the contract. Constructs
                       that 3.0 cannot express are translated or dropped; every
                       such change is printed. Decimals stay strings.

``break-decimal <contract>``
                       Same content with the ``Decimal`` schema's
                       ``type: string`` replaced by ``type: number``, for the
                       deliberate-failure run of ``check.sh --self-test``
                       (Rust path).

``break-identifier <contract>``
                       Same content with the ``Identifier`` schema's
                       ``type: string, pattern: ...`` replaced by
                       ``type: number``. Decimal itself is referenced nowhere
                       in the contract, so the Kiota self-test breaks
                       Identifier (the most typical string-with-pattern type)
                       and expects the generated C# to switch from ``string``
                       to a numeric CLR type.

The converted document is a build input only and is never committed (see
tools/contract-clients/README.md).
"""

from __future__ import annotations

import argparse
import copy
import json
import sys
from pathlib import Path
from typing import Any

import yaml

DROPPED: list[str] = []
TRANSLATED: list[str] = []
# Every Schema Object reached by the conversion, recorded with its document
# pointer after translation. check_translated() re-inspects exactly this list,
# so a schema location the conversion misses cannot slip past the checker.
ALL_SCHEMAS: list[tuple[str, Any]] = []

# Keys that exist in OpenAPI 3.1 Schema Objects but not in the OpenAPI 3.0
# Schema Object. Dropping them constrains *generated* code only (pattern and
# shape metadata), never a field type; see README.md.
SCHEMA_ONLY_31_KEYS = (
    "examples",
    "propertyNames",
    "patternProperties",
    "contains",
    "if",
    "then",
    "else",
    "dependentSchemas",
)

SCHEMA_LIST_KEYS = ("allOf", "anyOf", "oneOf")
SCHEMA_SINGLE_KEYS = ("items", "additionalProperties", "not")


def resolve_ref(root: dict[str, Any], ref: str) -> Any:
    """Resolve a local JSON Reference (``#/...``) against ``root``.

    Only references into this document are supported: the generators get one
    self-contained file, so a remote or file reference would silently produce a
    different contract. ``#/a/b`` is an RFC 6901 JSON Pointer over the document,
    so the ``#/`` prefix is dropped before splitting and the ``~1`` (``/``) and
    ``~0`` (``~``) escapes are undone in that order.
    """
    if not ref.startswith("#/"):
        raise SystemExit(
            f"convert.py: only local '#/...' references can be resolved; got {ref!r}. "
            "The converted document must be self-contained."
        )
    node: Any = root
    for part in ref[2:].split("/"):
        token = part.replace("~1", "/").replace("~0", "~")
        try:
            if isinstance(node, list):
                # JSON Pointer addresses array members by index.
                node = node[int(token)]
            else:
                node = node[token]
        except (KeyError, IndexError, TypeError, ValueError):
            raise SystemExit(
                f"convert.py: cannot resolve reference {ref!r}: no {token!r} at "
                f".../{part!r} in the document"
            ) from None
    return node


def fix_schema(node: Any, ctx: str, root: dict[str, Any]) -> Any:
    """Translate one Schema Object from OpenAPI 3.1 to 3.0."""
    if not isinstance(node, dict):
        return node

    # 3.1 allows keyword siblings next to a $ref; 3.0 does not. Inline the
    # referenced schema and overlay the siblings. The contract currently has
    # none of these; the rule exists so a future one does not silently pass.
    if "$ref" in node and len(node) > 1:
        siblings = {k: v for k, v in node.items() if k != "$ref"}
        target = resolve_ref(root, node["$ref"])
        merged = copy.deepcopy(target)
        merged.update(siblings)
        DROPPED.append(f"$ref sibling inlined at {ctx}")
        node = merged

    if "const" in node:
        # `const` is 3.1-only; 3.0 spells a single permitted value as enum.
        node["enum"] = [node.pop("const")]
        TRANSLATED.append(f"const -> enum at {ctx}")

    spec_type = node.get("type")
    if isinstance(spec_type, list):
        non_null = [t for t in spec_type if t != "null"]
        has_null = len(non_null) != len(spec_type)
        if len(non_null) == 1 and has_null:
            # `type: [string, "null"]` -> 3.0's type + nullable.
            node["type"] = non_null[0]
            node["nullable"] = True
            TRANSLATED.append(f"{ctx}: type array -> {non_null[0]} + nullable")
        else:
            # A JSON value of several types (the storage `Value`, the realtime
            # event payloads) is arbitrary JSON on the client: it is decoded
            # into serde_json::Value (Rust) or Kiota's untyped node types (C#).
            # The per-type list carries nothing a generated client could use,
            # so it collapses to an unconstrained schema ({}); keep the text.
            keep = {k: node[k] for k in ("description", "title") if k in node}
            node.clear()
            node.update(keep)
            DROPPED.append(f"{ctx}: type-union {spec_type} -> unconstrained schema")

    for key in SCHEMA_ONLY_31_KEYS:
        if key in node:
            del node[key]
            DROPPED.append(f"{ctx}.{key} dropped (not in OpenAPI 3.0)")

    for key in SCHEMA_LIST_KEYS:
        if key in node and isinstance(node[key], list):
            node[key] = [fix_schema(item, f"{ctx}/{key}", root) for item in node[key]]
    for key in SCHEMA_SINGLE_KEYS:
        if key in node and isinstance(node[key], dict):
            node[key] = fix_schema(node[key], f"{ctx}/{key}", root)
    for key, prop in node.get("properties", {}).items():
        node["properties"][key] = fix_schema(prop, f"{ctx}.{key}", root)
    ALL_SCHEMAS.append((ctx, node))
    return node


def fix_content(content: dict[str, Any], ctx: str, root: dict[str, Any]) -> None:
    """`content` is a Media Type Object map; only its `schema` is a Schema."""
    for media, mt in content.items():
        if isinstance(mt, dict) and "schema" in mt:
            mt["schema"] = fix_schema(mt["schema"], f"{ctx}:{media}/schema", root)


def fix_parameter(param: dict[str, Any], ctx: str, root: dict[str, Any]) -> None:
    if isinstance(param, dict) and "schema" in param:
        param["schema"] = fix_schema(param["schema"], f"{ctx}/schema", root)
    if isinstance(param, dict) and "content" in param:
        fix_content(param["content"], ctx, root)


def fix_response(resp: dict[str, Any], ctx: str, root: dict[str, Any]) -> None:
    if not isinstance(resp, dict):
        return
    if "content" in resp:
        fix_content(resp["content"], ctx, root)
    for head, hdr in resp.get("headers", {}).items():
        if isinstance(hdr, dict) and "schema" in hdr:
            hdr["schema"] = fix_schema(hdr["schema"], f"{ctx}/headers/{head}/schema", root)


ERROR_REF = "#/components/schemas/ErrorEnvelope"  # the response body of "Error"

# Request-body media types progenitor can type into a method parameter. It
# refuses anything else ("unexpected content type") and refuses an operation
# with more than one ("more media types than expected"); multipart/form-data
# is not representable at all.
PROGENITOR_REQUEST_MEDIA_TYPES = (
    "application/octet-stream",
    "application/json",
    "application/x-www-form-urlencoded",
    "text/plain",
    "text/x-markdown",
)


def fold_request_body(body: Any, where: str) -> None:
    """Keep the one request-body media type progenitor can type.

    The contract offers the file upload in two encodings
    (``application/octet-stream`` and ``multipart/form-data``); progenitor
    panics on a body with two media types and cannot express multipart at all.
    The converted document keeps the first media type progenitor supports and
    drops the rest, logging each one on stderr, exactly as the readiness 503
    response is dropped below. The C# client, generated from the original 3.1
    contract, keeps every encoding. See README.md.
    """
    if not isinstance(body, dict):
        return
    content = body.get("content")
    if not isinstance(content, dict) or len(content) <= 1:
        return
    keep = next((m for m in PROGENITOR_REQUEST_MEDIA_TYPES if m in content), None)
    if keep is None:
        # Nothing here is typable: leave the body alone so progenitor reports
        # it with its own message instead of a silently incomplete operation.
        return
    for media in [m for m in content if m != keep]:
        del content[media]
        DROPPED.append(
            f"{where}: request body media type {media} dropped; progenitor types exactly "
            f"one request body and cannot express it, so the generated Rust client carries "
            f"{keep} only (the C# client, generated from the 3.1 contract, keeps both)"
        )


def fold_responses(op: dict[str, Any], where: str, root: dict[str, Any]) -> None:
    """Reshape an operation's responses for progenitor's response model.

    Progenitor is built for Dropshot-style specs: an operation has exactly one
    success body type and one error body type, with errors in `2XX`-style
    *ranges* beside the exact success codes. The contract declares one exact
    success status (200/201/204/101) and a `default` error response, which
    progenitor rejects (assertion failure on `response_types.len() <= 1` in
    progenitor-impl's method.rs), so the converted document says the same
    thing in shapes progenitor supports:

    - `default` becomes the `4XX` and `5XX` ranges. All runtime errors carry
      the Error response body, and no 1xx/3xx error statuses exist in this
      API, so this is a pure re-rendering.
    - An exact error status whose body is the Error response is folded into
      those ranges (identical body; the list is otherwise redundant).
    - An exact error response with a *different* body (the readiness 503
      returns the `Ready` check payload) is dropped from the converted
      document: progenitor cannot type two error bodies for one operation.
      The change is logged loudly on stderr; the C# client, generated from
      the original 3.1 contract, keeps the typed response. See README.md.
    """
    responses = op["responses"]
    if "default" in responses:
        default_resp = responses.pop("default")
        if isinstance(default_resp, dict) and "content" in default_resp:
            fix_content(default_resp["content"], f"{where}/default", root)
        responses["4XX"] = default_resp
        responses["5XX"] = copy.deepcopy(default_resp)
        TRANSLATED.append(f"{where}: 'default' -> '4XX' + '5XX' (single error body type)")

    for status in list(responses.keys()):
        if not (str(status).isdigit() and 400 <= int(str(status)) < 600):
            continue
        resp = responses[status]
        body = None
        if isinstance(resp, dict):
            media = resp.get("content", {}).get("application/json", {})
            body = media.get("schema", {}).get("$ref")
        if body == ERROR_REF:
            del responses[status]
            TRANSLATED.append(f"{where}: {status} folded into the 4XX/5XX ranges (same body)")
        else:
            del responses[status]
            DROPPED.append(
                f"{where}: response {status} body is not the Error response and progenitor "
                "needs exactly one error body type per operation; dropped from the converted "
                "document (the C# client, generated from the 3.1 contract, keeps it)"
            )
def check_translated(root: dict[str, Any]) -> None:
    """Re-inspect every translated Schema Object for 3.1-only constructs.

    Walking ``ALL_SCHEMAS`` (recorded by ``fix_schema`` itself) instead of the
    whole document keeps legal OpenAPI 3.0 constructs — for example Media Type
    Object ``examples`` — out of the check, while any schema the conversion
    misses cannot slip through.
    """
    problems: list[str] = []

    def assert_30(schema: dict[str, Any], ptr: str) -> None:
        if "const" in schema:
            problems.append(f"{ptr}: 'const' is not in OpenAPI 3.0 (translate to enum)")
        if isinstance(schema.get("type"), list):
            problems.append(f"{ptr}: 'type' lists are not in OpenAPI 3.0")
        for key in SCHEMA_ONLY_31_KEYS:
            if key in schema:
                problems.append(f"{ptr}: '{key}' is not in the OpenAPI 3.0 Schema Object")
        if "$ref" in schema and len(schema) > 1:
            problems.append(f"{ptr}: '$ref' may not carry sibling keywords in OpenAPI 3.0")
        for value in schema.values():
            if isinstance(value, dict):
                walk_schema(value, ptr)

    def walk_schema(node: Any, ptr: str) -> None:
        if isinstance(node, dict):
            assert_30(node, ptr)
        elif isinstance(node, list):
            for item in node:
                if isinstance(item, dict):
                    assert_30(item, ptr)

    for ptr, schema in ALL_SCHEMAS:
        assert_30(schema, ptr)

    # 3.1-only keys at the Document level (outside any Schema Object).
    for key in ("jsonSchemaDialect", "webhooks"):
        if key in root:
            problems.append(f"#/{key}: not in OpenAPI 3.0")

    if problems:
        raise SystemExit(
            "convert.py: 3.1-only constructs survived the conversion:\n  "
            + "\n  ".join(problems)
        )


def to_30(text: str) -> dict[str, Any]:
    DROPPED.clear()
    TRANSLATED.clear()
    ALL_SCHEMAS.clear()
    root = yaml.safe_load(text)
    if not isinstance(root, dict) or not str(root.get("openapi", "")).startswith("3."):
        raise SystemExit("convert.py: expected an OpenAPI 3.x document")

    root["openapi"] = "3.0.3"
    if "summary" in root.get("info", {}):
        del root["info"]["summary"]
        DROPPED.append("#/info/summary (field does not exist in OpenAPI 3.0)")
    root.pop("jsonSchemaDialect", None)

    components = root.get("components", {})
    for name, schema in components.get("schemas", {}).items():
        components["schemas"][name] = fix_schema(schema, f"#/components/schemas/{name}", root)
    for name, param in components.get("parameters", {}).items():
        fix_parameter(param, f"#/components/parameters/{name}", root)
    for name, header in components.get("headers", {}).items():
        if isinstance(header, dict) and "schema" in header:
            header["schema"] = fix_schema(
                header["schema"], f"#/components/headers/{name}/schema", root
            )
    for name, body in components.get("requestBodies", {}).items():
        fold_request_body(body, f"#/components/requestBodies/{name}")
        fix_content(body.get("content", {}), f"#/components/requestBodies/{name}", root)
    for name, resp in components.get("responses", {}).items():
        fix_response(resp, f"#/components/responses/{name}", root)

    for path, item in root.get("paths", {}).items():
        if "parameters" in item:
            for i, param in enumerate(item["parameters"]):
                fix_parameter(param, f"paths:{path}/parameters/{i}", root)
        for method, op in item.items():
            if method in ("get", "put", "post", "delete", "options", "head", "patch", "trace"):
                for i, param in enumerate(op.get("parameters", [])):
                    fix_parameter(param, f"paths:{path}:{method}/parameters/{i}", root)
                if "requestBody" in op:
                    body = op["requestBody"]
                    fold_request_body(body, f"paths:{path}:{method}")
                    fix_content(
                        body.get("content", {}),
                        f"paths:{path}:{method}/requestBody",
                        root,
                    )
                for status, resp in op.get("responses", {}).items():
                    fix_response(resp, f"paths:{path}:{method}:{status}", root)
                if "responses" in op:
                    fold_responses(op, f"paths:{path}:{method}", root)

    check_translated(root)
    return root


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("mode", choices=("to-30", "break-decimal", "break-identifier"))
    parser.add_argument("contract", type=Path)
    parser.add_argument("--output", type=Path, default=None)
    parser.add_argument("--format", choices=("json", "yaml"), default=None)
    args = parser.parse_args()

    text = args.contract.read_text(encoding="utf-8")

    if args.mode == "break-decimal":
        doc = yaml.safe_load(text)
        schema = doc["components"]["schemas"]["Decimal"]
        if schema.get("type") != "string":
            raise SystemExit(
                "convert.py: expected the contract's Decimal schema to be type: string; "
                f"it is {schema.get('type')!r}, so the exactness self-test would prove "
                "nothing (the contract changed; review tools/contract-clients/check.sh)"
            )
        schema["type"] = "number"
        schema.pop("pattern", None)
        out_doc, out_fmt = doc, "yaml"
    elif args.mode == "break-identifier":
        doc = yaml.safe_load(text)
        schema = doc["components"]["schemas"]["Identifier"]
        if schema.get("type") != "string" or "pattern" not in schema:
            raise SystemExit(
                "convert.py: expected the contract's Identifier schema to be "
                f"type: string with a pattern; it is ({schema.get('type')!r}, "
                "pattern: ...) so the exactness self-test would prove nothing "
                "(the contract changed; review tools/contract-clients/check.sh)"
            )
        schema["type"] = "number"
        schema.pop("pattern", None)
        schema["description"] = "MUTANT: Identifier is numeric"
        out_doc, out_fmt = doc, "yaml"
    else:
        out_doc = to_30(text)
        out_fmt = "json"

    out_fmt = args.format or out_fmt
    rendered = (
        json.dumps(out_doc, indent=2)
        if out_fmt == "json"
        else yaml.safe_dump(out_doc, sort_keys=False)
    )
    if args.output is None:
        sys.stdout.write(rendered)
    else:
        args.output.write_text(rendered, encoding="utf-8")

    for line in DROPPED:
        print(f"convert.py: DROPPED {line}", file=sys.stderr)
    for line in TRANSLATED:
        print(f"convert.py: TRANSLATED {line}", file=sys.stderr)


if __name__ == "__main__":
    main()