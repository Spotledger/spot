#!/usr/bin/env python3
"""Contract checks for the native API (WP-CON-03, ADR-055).

Run with the pinned requirements in ``requirements.txt`` (``tools/lints/native-api/run.sh``
creates the environment). Every rule below is part of the contract; a failure names the rule
and the JSON pointer it failed at.

1. ``openapi.yaml`` is a valid OpenAPI 3.1 document.
2. No schema anywhere uses ``type: number``: money never passes through a float (ADR-12).
3. Paths are ``/api/v1/...`` or ``/api/health/...``; operation ids are unique snake_case.
4. Every ``/api/v1`` operation has a ``default`` response referencing the shared Error.
5. The error-code registry (``x-spot-error-codes``) is well formed, and every top-level
   ``error.code`` used in an example is registered.
6. Every example validates against the schema it illustrates.
7. ``realtime.md`` documents every realtime message ``type`` defined in the schemas.
8. The lookup operation never takes a seam (ADR-068 §3.5): its request schema is closed and has
   no property naming a seam, and ``lookup.seam_failed`` is registered.
9. Every "<status> <code>" an operation description names (for example ``422 bulk.cursor_invalid``)
   is a registered code with exactly that HTTP status.
10. ``EventEnvelope`` and its parts have the property names of ``contracts/events`` (ADR-056):
    the stream carries the envelope the outbox defines, not a second shape.
11. Secrets and keys stay out of URLs and responses (ADR-055, WP-CON-03.T5): no query parameter is
    named like a token, cursor, watermark, secret or password; no response schema property names a
    database or storage key; the reveal operation takes one field of one document (a closed
    request with a single ``field`` and no batch form) and the system-only bulk reveal is not exposed.
12. The field-presentation inputs stay on the server's side of the line (WP-CON-03.T6): no schema
    describing a field or a lookup names a mask format (the server forms the masked text, the real
    value never reaches a client before ``field_reveal``), ``FieldHints.control`` is a closed
    enumeration, and ``revealable``, ``attach_limits``, ``Document._currency`` and ``multiple``
    exist where the permission, upload, currency and select inputs are carried.
13. Field rule expressions are bounded strings, ``expr_version`` travels with them, and the lookup
    request stays closed (WP-CON-03.T7).
14. Printing is a read-only ``GET`` that answers ``application/pdf`` only (WP-CON-03.T9): the
    operation takes no body and no other method, ``format`` is its only query parameter, and no
    response header or property names a path or a storage key.
15. The realtime ``notification`` vocabulary is named and bounded and defines no ``left`` message
    (WP-CON-03.T10).
16. Creation-only fields and typed principals (ADR-083, contract 0.9.0): ``FieldMeta.create_only``
    is a boolean, ``FieldType`` names ``consumer_ref``, and the actor ``kind`` of an event envelope
    is exactly the set of variants of ``Actor`` in ``contracts/events``.
17. Notices, ownership and consumer renewal (contract 0.10.0): ``realtime.md`` names the keys of a
    ``user_notice`` ``data`` object and the severities; ``partition.not_owner`` is a registered 403;
    ``notification_list`` is a ``GET`` whose only parameters are ``after`` and ``limit`` (at most
    100); ``auth_consumer_renew`` is a body-less ``POST`` that needs no bearer and whose description
    fixes the ``__Host-`` cookie and its ``HttpOnly``, ``Secure`` and ``SameSite=Strict`` attributes.
"""

from __future__ import annotations

import re
import sys
from pathlib import Path
from typing import Any, Iterator

import yaml
from jsonschema import Draft202012Validator
from openapi_spec_validator import validate as validate_openapi
from referencing import Registry, Resource
from referencing.jsonschema import DRAFT202012

HERE = Path(__file__).resolve().parent
EVENTS_SRC = HERE.parent / "events" / "src"
SPEC_PATH = HERE / "openapi.yaml"
REALTIME_PATH = HERE / "realtime.md"
ROOT_URI = "urn:spot:native-api"

CODE_RE = re.compile(r"^[a-z][a-z0-9_]*(\.[a-z][a-z0-9_]*)+$")
OPERATION_ID_RE = re.compile(r"^[a-z][a-z0-9_]*$")
HTTP_METHODS = ("get", "put", "post", "delete", "patch", "head", "options", "trace")


def pointer(parts: list[str | int]) -> str:
    return "/" + "/".join(str(p).replace("~", "~0").replace("/", "~1") for p in parts)


def walk(node: Any, path: list[str | int]) -> Iterator[tuple[list[str | int], Any]]:
    yield path, node
    if isinstance(node, dict):
        for k, v in node.items():
            yield from walk(v, path + [k])
    elif isinstance(node, list):
        for i, v in enumerate(node):
            yield from walk(v, path + [i])


def load() -> dict[str, Any]:
    with SPEC_PATH.open(encoding="utf-8") as f:
        return yaml.safe_load(f)


def check_openapi(spec: dict[str, Any], errors: list[str]) -> None:
    try:
        validate_openapi(spec)
    except Exception as exc:  # the validator raises several exception types
        errors.append(f"rule 1: not a valid OpenAPI 3.1 document: {exc}")
    if not str(spec.get("openapi", "")).startswith("3.1"):
        errors.append("rule 1: `openapi` must be 3.1.x")


def check_no_number(spec: dict[str, Any], errors: list[str]) -> None:
    for path, node in walk(spec, []):
        if isinstance(node, dict) and "type" in node:
            t = node["type"]
            types = t if isinstance(t, list) else [t]
            if "number" in types:
                errors.append(f"rule 2: `type: number` at {pointer(path)}; decimals are strings")


def operations(spec: dict[str, Any]) -> Iterator[tuple[str, str, dict[str, Any]]]:
    for route, item in spec.get("paths", {}).items():
        for method in HTTP_METHODS:
            if method in item:
                yield route, method, item[method]


def check_paths(spec: dict[str, Any], errors: list[str]) -> None:
    seen: dict[str, str] = {}
    for route, method, op in operations(spec):
        where = f"{method.upper()} {route}"
        if not (route.startswith("/api/v1/") or route.startswith("/api/health/")):
            errors.append(f"rule 3: {where}: paths start with /api/v1/ or /api/health/")
        op_id = op.get("operationId")
        if not op_id or not OPERATION_ID_RE.fullmatch(op_id):
            errors.append(f"rule 3: {where}: operationId must be snake_case, got {op_id!r}")
        elif op_id in seen:
            errors.append(f"rule 3: {where}: operationId {op_id} also used by {seen[op_id]}")
        else:
            seen[op_id] = where
        if route.startswith("/api/v1/"):
            default = op.get("responses", {}).get("default", {})
            if default.get("$ref") != "#/components/responses/Error":
                errors.append(f"rule 4: {where}: needs `default: $ref #/components/responses/Error`")


def check_registry(spec: dict[str, Any], errors: list[str]) -> set[str]:
    registry = spec.get("x-spot-error-codes")
    if not isinstance(registry, dict) or not registry:
        errors.append("rule 5: x-spot-error-codes is missing or empty")
        return set()
    for code, entry in registry.items():
        if not CODE_RE.fullmatch(code):
            errors.append(f"rule 5: error code {code!r} is not dotted lower-case")
        ok = (isinstance(entry, list) and len(entry) == 3 and isinstance(entry[0], int)
              and 400 <= entry[0] <= 599 and isinstance(entry[1], bool)
              and isinstance(entry[2], str) and entry[2])
        if not ok:
            errors.append(f"rule 5: {code}: entry must be [status 400-599, retryable, meaning]")
    return set(registry)


def examples(spec: dict[str, Any]) -> Iterator[tuple[str, list[str | int], Any]]:
    """Yields (schema pointer, example path, example value) for every example with a schema."""
    for path, node in walk(spec, []):
        if not isinstance(node, dict):
            continue
        # A media type object: {schema, example | examples: {name: {value}}}.
        if "schema" in node and isinstance(node["schema"], dict):
            schema_ptr = pointer(path + ["schema"])
            if "example" in node:
                yield schema_ptr, path + ["example"], node["example"]
            for name, ex in (node.get("examples") or {}).items():
                if isinstance(ex, dict) and "value" in ex:
                    yield schema_ptr, path + ["examples", name, "value"], ex["value"]
        # A schema with a JSON Schema `examples` array.
        if path[:2] == ["components", "schemas"] and len(path) == 3:
            for i, ex in enumerate(node.get("examples") or []):
                yield pointer(path), path + ["examples", i], ex


def check_examples(spec: dict[str, Any], codes: set[str], errors: list[str]) -> int:
    registry = Registry().with_resource(
        ROOT_URI, Resource.from_contents(spec, default_specification=DRAFT202012))
    count = 0
    for schema_ptr, ex_path, value in examples(spec):
        count += 1
        validator = Draft202012Validator({"$ref": f"{ROOT_URI}#{schema_ptr}"}, registry=registry)
        for err in validator.iter_errors(value):
            loc = pointer(ex_path + list(err.absolute_path))
            errors.append(f"rule 6: example at {loc} does not match {schema_ptr}: {err.message}")
        if isinstance(value, dict) and isinstance(value.get("error"), dict):
            code = value["error"].get("code")
            if code not in codes:
                errors.append(f"rule 5: example at {pointer(ex_path)} uses unregistered code {code!r}")
    if count == 0:
        errors.append("rule 6: no examples found; the check is not looking in the right place")
    return count


def check_realtime_doc(spec: dict[str, Any], errors: list[str], text: str | None = None) -> None:
    if text is None:
        if not REALTIME_PATH.is_file():
            errors.append("rule 7: realtime.md is missing")
            return
        text = REALTIME_PATH.read_text(encoding="utf-8")
    schemas = spec.get("components", {}).get("schemas", {})
    for name in ("RealtimeClientMessage", "RealtimeServerMessage"):
        for variant in schemas.get(name, {}).get("oneOf", []):
            t = variant.get("properties", {}).get("type", {}).get("const")
            if t and f"`{t}`" not in text:
                errors.append(f"rule 7: realtime.md does not document message type `{t}` ({name})")


def check_lookup(spec: dict[str, Any], codes: set[str], errors: list[str]) -> None:
    route = spec.get("paths", {}).get("/api/v1/lookup/{doctype}/{field}", {})
    if "post" not in route:
        errors.append("rule 8: POST /api/v1/lookup/{doctype}/{field} is missing")
    req = spec.get("components", {}).get("schemas", {}).get("LookupRequest", {})
    if req.get("additionalProperties") is not False:
        errors.append("rule 8: LookupRequest must set additionalProperties: false")
    for name in req.get("properties", {}):
        if "seam" in name.lower():
            errors.append(f"rule 8: LookupRequest property {name!r}: a client never names a seam")
    if "lookup.seam_failed" not in codes:
        errors.append("rule 8: error code lookup.seam_failed is not registered")


STATUS_CODE_RE = re.compile(r"`(\d{3}) ([a-z][a-z0-9_]*(?:\.[a-z][a-z0-9_]*)+)`")


def check_described_codes(spec: dict[str, Any], errors: list[str]) -> None:
    registry = spec.get("x-spot-error-codes", {})
    for route, method, op in operations(spec):
        text = " ".join(str(op.get("description", "")).split())
        for status, code in STATUS_CODE_RE.findall(text):
            entry = registry.get(code)
            where = f"{method.upper()} {route}"
            if entry is None:
                errors.append(f"rule 9: {where}: description names unregistered code {code!r}")
            elif entry[0] != int(status):
                errors.append(f"rule 9: {where}: description says {status} {code} but the registry says {entry[0]}")


def rust_struct_fields(source: str, name: str) -> set[str]:
    match = re.search(r"pub struct " + name + r"\s*\{(.*?)\n\}", source, re.S)
    if not match:
        return set()
    return set(re.findall(r"^\s*pub (\w+):", match.group(1), re.M))


def check_event_envelope(spec: dict[str, Any], errors: list[str], src_dir: Path | None = None) -> None:
    src_dir = src_dir or EVENTS_SRC
    envelope_rs = src_dir / "envelope.rs"
    payload_rs = src_dir / "payload.rs"
    if not envelope_rs.is_file() or not payload_rs.is_file():
        errors.append("rule 10: contracts/events/src/{envelope,payload}.rs not found")
        return
    envelope, payload = envelope_rs.read_text(encoding="utf-8"), payload_rs.read_text(encoding="utf-8")
    schema = spec.get("components", {}).get("schemas", {}).get("EventEnvelope", {})
    props = schema.get("properties", {})
    pairs = [
        ("EventEnvelope", set(props), rust_struct_fields(envelope, "EventEnvelope")),
        ("EventEnvelope.position", set(props.get("position", {}).get("properties", {})), rust_struct_fields(envelope, "Position")),
        ("EventEnvelope.subject", set(props.get("subject", {}).get("properties", {})), rust_struct_fields(envelope, "Subject")),
        ("EventEnvelope.meta", set(props.get("meta", {}).get("properties", {})), rust_struct_fields(payload, "EventMeta")),
    ]
    for label, in_spec, in_rust in pairs:
        if not in_rust:
            errors.append(f"rule 10: could not read the {label} fields from contracts/events")
        elif in_spec != in_rust:
            errors.append(f"rule 10: {label} differs from contracts/events: only in openapi "
                          f"{sorted(in_spec - in_rust)}, only in contracts/events {sorted(in_rust - in_spec)}")


SECRET_PARAM_RE = re.compile(r"token|cursor|watermark|secret|password|api_?key|credential", re.I)
# The document list's keyset paging cursor (0.1.0) is a page position of a read the caller is
# already allowed to make; it grants nothing. It is the one cursor that stays in a query string.
PAGING_CURSOR_IN_QUERY = {("resource_list", "cursor")}
STORAGE_KEY_RE = re.compile(r"^(storage|database|db)_(key|path|id|name)$|^(storage|db)_", re.I)


def check_secrets(spec: dict[str, Any], errors: list[str]) -> None:
    for route, method, op in operations(spec):
        where = f"{method.upper()} {route}"
        params = list(spec["paths"][route].get("parameters", [])) + list(op.get("parameters", []))
        for param in params:
            if "$ref" in param:
                param = spec["components"]["parameters"][param["$ref"].rsplit("/", 1)[-1]]
            if (param.get("in") == "query" and SECRET_PARAM_RE.search(param.get("name", ""))
                    and (op.get("operationId"), param.get("name")) not in PAGING_CURSOR_IN_QUERY):
                errors.append(f"rule 11: {where}: query parameter {param['name']!r} would put a secret or cursor in a URL")
    for path, node in walk(spec.get("components", {}).get("schemas", {}), ["components", "schemas"]):
        if isinstance(node, dict) and isinstance(node.get("properties"), dict):
            for prop in node["properties"]:
                if STORAGE_KEY_RE.search(prop):
                    errors.append(f"rule 11: {pointer(path)}: property {prop!r} names a database or storage key")
    for route, method, op in operations(spec):
        if "reveal_bulk" in route or "reveal_bulk" in str(op.get("operationId", "")):
            errors.append(f"rule 11: {method.upper()} {route}: the system-only bulk reveal is not part of the API")
    reveal = spec.get("paths", {}).get("/api/v1/resource/{doctype}/{id}/reveal", {}).get("post")
    request = spec.get("components", {}).get("schemas", {}).get("RevealRequest", {})
    if reveal is None:
        errors.append("rule 11: POST /api/v1/resource/{doctype}/{id}/reveal is missing")
    elif (request.get("additionalProperties") is not False
          or set(request.get("properties", {})) != {"field", "reason"}
          or request.get("properties", {}).get("field", {}).get("type") == "array"):
        errors.append("rule 11: RevealRequest must be closed with exactly `field` and `reason` (one field, no batch)")


def check_presentation(spec: dict[str, Any], errors: list[str]) -> None:
    schemas = spec.get("components", {}).get("schemas", {})
    for name in ("FieldMeta", "FieldHints", "LookupSpec", "LookupRow", "CurrencyInfo"):
        for prop in schemas.get(name, {}).get("properties", {}):
            if re.search(r"mask", prop, re.IGNORECASE):
                errors.append(f"rule 12: {name}.{prop}: a mask format must not reach a client (the server forms the masked text)")
    control = schemas.get("FieldHints", {}).get("properties", {}).get("control", {})
    if not control.get("enum"):
        errors.append("rule 12: FieldHints.control must be a closed, non-empty enumeration")
    perms = schemas.get("DoctypePermissions", {}).get("properties", {})
    if "revealable" not in perms.get("fields", {}).get("properties", {}):
        errors.append("rule 12: DoctypePermissions.fields.revealable is missing")
    if "attach_limits" not in perms:
        errors.append("rule 12: DoctypePermissions.attach_limits is missing")
    if "_currency" not in schemas.get("Document", {}).get("properties", {}):
        errors.append("rule 12: Document._currency is missing")
    if "multiple" not in schemas.get("FieldMeta", {}).get("properties", {}):
        errors.append("rule 12: FieldMeta.multiple is missing")


RULE_EXPRESSIONS = ("depends_on", "mandatory_depends_on", "read_only_depends_on", "default_expr")


def check_expressions(spec: dict[str, Any], errors: list[str]) -> None:
    """Rule 13 (WP-CON-03.T7): field rule expressions are bounded strings, the language version
    travels with them, and the lookup request stays closed (the server applies the filter)."""
    schemas = spec.get("components", {}).get("schemas", {})
    meta = schemas.get("FieldMeta", {}).get("properties", {})
    for name in RULE_EXPRESSIONS:
        prop = meta.get(name)
        if not isinstance(prop, dict) or prop.get("type") != "string":
            errors.append(f"rule 13: FieldMeta.{name} must be a string property")
        elif not isinstance(prop.get("maxLength"), int) or prop["maxLength"] > 4096:
            errors.append(f"rule 13: FieldMeta.{name} must have a maxLength of at most 4096")
    version = schemas.get("DocTypeMeta", {}).get("properties", {}).get("expr_version", {})
    if version.get("type") != "integer" or version.get("minimum", 0) < 1:
        errors.append("rule 13: DocTypeMeta.expr_version must be an integer of at least 1")
    request = schemas.get("LookupRequest", {})
    if request.get("additionalProperties") is not False or any(
            re.search(r"filter", p, re.IGNORECASE) for p in request.get("properties", {})):
        errors.append("rule 13: LookupRequest must stay closed and carry no filter (the server applies it)")
    if "provides" not in schemas.get("LookupRow", {}).get("properties", {}):
        errors.append("rule 13: LookupRow.provides is missing")
    # Every example DocTypeMeta that carries an expression names its language version.
    for path, node in walk(spec, []):
        if not isinstance(node, dict) or not isinstance(node.get("fields"), list):
            continue
        fields = [f for f in node["fields"] if isinstance(f, dict)]
        carried = [(f.get("name"), k) for f in fields for k in RULE_EXPRESSIONS if k in f]
        if carried and "expr_version" not in node:
            errors.append(f"rule 13: {pointer(path)}: carries expressions {carried[:2]} but no expr_version")
        for fname, key in carried:
            field = next((f for f in fields if f.get("name") == fname), {})
            if not isinstance(field.get(key), str) or len(field[key]) > 1024:
                errors.append(f"rule 13: {pointer(path)}: {fname}.{key} must be a string of at most 1024 characters")


PRINT_ROUTE = "/api/v1/print/{doctype}/{id}"
# A response header or property that would leak where the bytes live (WP-CON-03.T9): the
# client gets the PDF body, never a file name, a path or a storage key.
PATH_OR_STORAGE_KEY_RE = re.compile(r"path|storage|database|(^|_)key$|^db_", re.I)


def check_print(spec: dict[str, Any], codes: set[str], errors: list[str]) -> None:
    """Rule 14 (WP-CON-03.T9): printing is a read-only GET that answers a PDF and nothing else."""
    item = spec.get("paths", {}).get(PRINT_ROUTE)
    if not isinstance(item, dict) or "get" not in item:
        errors.append(f"rule 14: GET {PRINT_ROUTE} is missing")
        return
    for method in HTTP_METHODS:
        if method != "get" and method in item:
            errors.append(f"rule 14: {method.upper()} {PRINT_ROUTE}: printing is read-only; the path answers GET only")
    op = item["get"]
    if "requestBody" in op:
        errors.append(f"rule 14: GET {PRINT_ROUTE}: a read-only operation takes no request body")
    query: set[str] = set()
    for param in list(item.get("parameters") or []) + list(op.get("parameters") or []):
        if "$ref" in param:
            param = spec["components"]["parameters"][param["$ref"].rsplit("/", 1)[-1]]
        if param.get("in") == "query":
            query.add(param.get("name"))
    if query != {"format"}:
        errors.append(f"rule 14: GET {PRINT_ROUTE}: `format` is the only query parameter, got {sorted(query)}")
    responses = op.get("responses") or {}
    if not isinstance(responses.get("200"), dict):
        errors.append(f"rule 14: GET {PRINT_ROUTE}: a 200 response is missing")
    for status, response in responses.items():
        for media in (response.get("content") or {}):
            if media != "application/pdf":
                errors.append(f"rule 14: GET {PRINT_ROUTE}: response {status} carries {media!r}; "
                              "a print is a PDF only")
    base = ["paths", PRINT_ROUTE, "get", "responses"]
    for path, node in walk(responses, []):
        if not isinstance(node, dict):
            continue
        for group in ("headers", "properties"):
            for name in (node.get(group) or {}):
                if PATH_OR_STORAGE_KEY_RE.search(name):
                    errors.append(f"rule 14: {pointer(base + path + [group, name])}: {name!r} names a path "
                                  "or a storage key; the client gets the bytes, not where they live")
    for code in ("print.format_unknown", "print.render_failed"):
        if code not in codes:
            errors.append(f"rule 14: error code {code} is not registered")


def rust_enum_variants(source: str, name: str) -> set[str]:
    match = re.search(r"pub enum " + name + r"\s*\{(.*?)\n\}", source, re.S)
    if not match:
        return set()
    # Struct-like variants only, at the enum's own indent: `    User {`.
    names = re.findall(r"^ {4}([A-Z]\w*)\s*\{", match.group(1), re.M)
    return {re.sub(r"(?<!^)(?=[A-Z])", "_", n).lower() for n in names}


def check_principals(spec: dict[str, Any], errors: list[str], src_dir: Path | None = None) -> None:
    """Rule 16: create-only fields and typed principals."""
    schemas = spec.get("components", {}).get("schemas", {})
    create_only = schemas.get("FieldMeta", {}).get("properties", {}).get("create_only", {})
    if create_only.get("type") != "boolean":
        errors.append("rule 16: FieldMeta.create_only must be a boolean property")
    if "consumer_ref" not in schemas.get("FieldType", {}).get("enum", []):
        errors.append("rule 16: FieldType must name consumer_ref")
    payload_rs = (src_dir or EVENTS_SRC) / "payload.rs"
    if not payload_rs.is_file():
        errors.append("rule 16: contracts/events/src/payload.rs not found")
        return
    kinds = rust_enum_variants(payload_rs.read_text(encoding="utf-8"), "Actor")
    actor = (schemas.get("EventEnvelope", {}).get("properties", {}).get("meta", {})
             .get("properties", {}).get("actor", {}).get("properties", {}).get("kind", {}))
    if not kinds:
        errors.append("rule 16: could not read the variants of Actor from contracts/events")
    elif set(actor.get("enum", [])) != kinds:
        errors.append(f"rule 16: EventEnvelope.meta.actor.kind differs from Actor in contracts/events: "
                      f"openapi {sorted(actor.get('enum', []))}, contracts/events {sorted(kinds)}")


NOTICE_ROUTE = "/api/v1/notifications"
RENEW_ROUTE = "/api/v1/auth/consumer/renew"
NOTICE_KEYS = ("title", "body", "severity", "doctype", "id", "kind")
NOTICE_SEVERITIES = ("info", "warning", "error")
RENEW_WORDS = ("__Host-", "HttpOnly", "Secure", "SameSite=Strict", "auth.session_expired")


def check_notices_and_renewal(spec: dict[str, Any], realtime_text: str | None, errors: list[str]) -> None:
    """Rule 17: the notice keys, `partition.not_owner`, the notice feed and the consumer renewal."""
    if realtime_text is not None:
        start = realtime_text.find("The keys of a `user_notice` `data` object")
        paragraph = realtime_text[start:].split("\n", 1)[0] if start >= 0 else ""
        if not paragraph:
            errors.append("rule 17: realtime.md does not list the keys of a `user_notice` data object")
        for key in NOTICE_KEYS:
            if paragraph and f"`{key}`" not in paragraph:
                errors.append(f"rule 17: realtime.md does not name the notice key `{key}`")
        for severity in NOTICE_SEVERITIES:
            if paragraph and f'"{severity}"' not in paragraph:
                errors.append(f"rule 17: realtime.md does not name the severity \"{severity}\"")
    entry = spec.get("x-spot-error-codes", {}).get("partition.not_owner")
    if not entry or entry[0] != 403:
        errors.append("rule 17: partition.not_owner must be registered with status 403")
    paths = spec.get("paths", {})
    feed = paths.get(NOTICE_ROUTE, {}).get("get")
    if not feed or feed.get("operationId") != "notification_list":
        errors.append(f"rule 17: GET {NOTICE_ROUTE} (notification_list) is missing")
    else:
        params = {p.get("name"): p for p in feed.get("parameters", [])}
        if set(params) != {"after", "limit"} or set(paths[NOTICE_ROUTE]) - {"get", "parameters"}:
            errors.append("rule 17: notification_list takes exactly the query parameters after and limit, and only GET")
        elif params["limit"].get("schema", {}).get("maximum") != 100:
            errors.append("rule 17: notification_list limit must be at most 100")
    renew = paths.get(RENEW_ROUTE, {}).get("post")
    if not renew or renew.get("operationId") != "auth_consumer_renew":
        errors.append(f"rule 17: POST {RENEW_ROUTE} (auth_consumer_renew) is missing")
        return
    if "requestBody" in renew or renew.get("security") != []:
        errors.append("rule 17: auth_consumer_renew takes no body and no bearer token")
    if any(p.get("in") == "query" for p in renew.get("parameters", [])):
        errors.append("rule 17: auth_consumer_renew takes no query parameter (no token in a URL)")
    text = renew.get("description", "")
    for word in RENEW_WORDS:
        if word not in text:
            errors.append(f"rule 17: auth_consumer_renew does not state {word}")


def check_notification(text: str, errors: list[str]) -> None:
    """Rule 15: the `notification` vocabulary is named, bounded, and adds no `left` message."""
    for needle, what in (("user_notice", "the `user_notice` event"), ("2 KiB", "the 2 KiB size limit"),
                         ("256 bytes", "the 256-byte string limit")):
        if needle not in text:
            errors.append(f"rule 15: realtime.md does not name {what}")
    if re.search(r"`left`\s*\*\*|\*\*`left`\*\*|- \*\*`left`", text):
        errors.append("rule 15: realtime.md defines a `left` message; none is in the contract")


def run_rules(spec: dict[str, Any], realtime_text: str | None = None) -> tuple[list[str], int, int]:
    errors: list[str] = []
    check_openapi(spec, errors)
    check_no_number(spec, errors)
    check_paths(spec, errors)
    codes = check_registry(spec, errors)
    n = check_examples(spec, codes, errors)
    check_realtime_doc(spec, errors, realtime_text)
    check_lookup(spec, codes, errors)
    check_described_codes(spec, errors)
    check_event_envelope(spec, errors)
    check_secrets(spec, errors)
    check_presentation(spec, errors)
    check_expressions(spec, errors)
    check_print(spec, codes, errors)
    check_principals(spec, errors)
    if realtime_text is None and REALTIME_PATH.is_file():
        realtime_text = REALTIME_PATH.read_text(encoding="utf-8")
    check_notices_and_renewal(spec, realtime_text, errors)
    if realtime_text is not None:
        check_notification(realtime_text, errors)
    return errors, len(codes), n


def self_test() -> int:
    """Breaks a copy of the spec once per rule and checks that exactly that rule fires."""
    import copy

    base = load()

    def broken(mutate):
        spec = copy.deepcopy(base)
        mutate(spec)
        return spec

    schemas = lambda s: s["components"]["schemas"]  # noqa: E731
    cases = {
        "rule 1": broken(lambda s: s.pop("info")),
        "rule 2": broken(lambda s: schemas(s)["Decimal"].update(type="number")),
        "rule 3": broken(lambda s: s["paths"]["/api/v1/auth/logout"]["post"].update(operationId="auth_login")),
        "rule 4": broken(lambda s: s["paths"]["/api/v1/auth/logout"]["post"]["responses"].pop("default")),
        "rule 5": broken(lambda s: s["x-spot-error-codes"].pop("document.stale_version")),
        "rule 6": broken(lambda s: schemas(s)["RealtimeClientMessage"]["examples"].append({"type": "join"})),
        "rule 8": broken(lambda s: schemas(s)["LookupRequest"]["properties"].update(seam={"type": "string"})),
        "rule 9": broken(lambda s: s["x-spot-error-codes"].update({"bulk.watermark_expired": [422, False, "x"]})),
        "rule 10": broken(lambda s: schemas(s)["EventEnvelope"]["properties"].update(shard={"type": "integer"})),
        "rule 11": broken(lambda s: s["paths"]["/api/v1/events/stream"]["get"]["parameters"].append(
            {"name": "cursor", "in": "query", "required": False, "schema": {"type": "string"}})),
        "rule 12": broken(lambda s: schemas(s)["FieldMeta"]["properties"].update(mask_format={"type": "string"})),
        "rule 13": broken(lambda s: schemas(s)["LookupRequest"]["properties"].update(filters={"type": "object"})),
        "rule 14": broken(lambda s: s["paths"][PRINT_ROUTE]["get"]["responses"]["200"]["headers"].update(
            {"X-Pdf-Path": {"schema": {"type": "string"}}})),
        "rule 16": broken(lambda s: schemas(s)["EventEnvelope"]["properties"]["meta"]["properties"]["actor"][
            "properties"]["kind"].update(enum=["user", "system"])),
    }
    failures = 0
    for rule, spec in cases.items():
        errors, _, _ = run_rules(spec)
        if not any(e.startswith(rule) for e in errors):
            print(f"self-test: breaking {rule} was not detected; got {errors[:3]}")
            failures += 1
    # The print rule has four parts; break each one on its own.
    print_breaks = {
        "a second method on the path": lambda s: s["paths"][PRINT_ROUTE].update(
            {"post": {"operationId": "print_post", "responses": {"default": {"$ref": "#/components/responses/Error"}}}}),
        "a second query parameter": lambda s: s["paths"][PRINT_ROUTE]["get"]["parameters"].append(
            {"name": "storage_path", "in": "query", "required": False, "schema": {"type": "string"}}),
        "a non-PDF response": lambda s: s["paths"][PRINT_ROUTE]["get"]["responses"]["200"]["content"].update(
            {"application/json": {"schema": {"type": "string"}}}),
        "a response property naming a path": lambda s: s["paths"][PRINT_ROUTE]["get"]["responses"]["200"]["content"][
            "application/pdf"].update({"schema": {"type": "object", "properties": {"file_path": {"type": "string"}}}}),
    }
    for label, mutate in print_breaks.items():
        errors, _, _ = run_rules(broken(mutate))
        if not any(e.startswith("rule 14") for e in errors):
            print(f"self-test: {label} (rule 14) was not detected; got {errors[:3]}")
            failures += 1
    principal_breaks = {
        "a missing create_only": lambda s: schemas(s)["FieldMeta"]["properties"].pop("create_only", None),
        "a missing consumer_ref type": lambda s: schemas(s)["FieldType"].update(
            enum=[t for t in schemas(s)["FieldType"]["enum"] if t != "consumer_ref"]),
        "a made-up actor kind": lambda s: schemas(s)["EventEnvelope"]["properties"]["meta"]["properties"][
            "actor"]["properties"]["kind"].update(enum=["user", "system", "consumer", "agent"]),
    }
    for label, mutate in principal_breaks.items():
        errors, _, _ = run_rules(broken(mutate))
        if not any(e.startswith("rule 16") for e in errors):
            print(f"self-test: {label} (rule 16) was not detected; got {errors[:3]}")
            failures += 1
    feed17 = {
        "an unregistered partition.not_owner": lambda s: s["x-spot-error-codes"].pop("partition.not_owner"),
        "a third notice parameter": lambda s: s["paths"][NOTICE_ROUTE]["get"]["parameters"].append(
            {"name": "user", "in": "query", "required": False, "schema": {"type": "string"}}),
        "an unbounded notice page": lambda s: s["paths"][NOTICE_ROUTE]["get"]["parameters"][1]["schema"].update(maximum=1000),
        "a renewal with a body": lambda s: s["paths"][RENEW_ROUTE]["post"].update(requestBody={"content": {}}),
        "a renewal needing a bearer": lambda s: s["paths"][RENEW_ROUTE]["post"].pop("security"),
        "a renewal with a query token": lambda s: s["paths"][RENEW_ROUTE]["post"].setdefault("parameters", []).append(
            {"name": "t", "in": "query", "schema": {"type": "string"}}),
        "a renewal without SameSite=Strict": lambda s: s["paths"][RENEW_ROUTE]["post"].update(
            description=s["paths"][RENEW_ROUTE]["post"]["description"].replace("SameSite=Strict", "SameSite=Lax")),
    }
    for label, mutate in feed17.items():
        errors, _, _ = run_rules(broken(mutate))
        if not any(e.startswith("rule 17") for e in errors):
            print(f"self-test: {label} (rule 17) was not detected; got {errors[:3]}")
            failures += 1
    good = REALTIME_PATH.read_text(encoding="utf-8")
    key_breaks = {
        "an unnamed notice key": good.replace("`severity` (", "`level` ("),
        "a missing severity value": good.replace('`"warning"`', "`warn`"),
        "no key list": good.replace("The keys of a `user_notice` `data` object", "Notice data"),
    }
    for label, text in key_breaks.items():
        errors, _, _ = run_rules(base, realtime_text=text)
        if not any(e.startswith("rule 17") for e in errors):
            print(f"self-test: {label} (rule 17) was not detected; got {errors[:3]}")
            failures += 1
    notice_breaks = {
        "an unnamed notice event": good.replace("user_notice", "notice"),
        "a missing size limit": good.replace("2 KiB", "a limit"),
        "a missing string limit": good.replace("256 bytes", "a limit"),
        "a `left` message": good + "\n- **`left`**: `{type, room}`.\n",
    }
    for label, text in notice_breaks.items():
        errors, _, _ = run_rules(base, realtime_text=text)
        if not any(e.startswith("rule 15") for e in errors):
            print(f"self-test: {label} (rule 15) was not detected; got {errors[:3]}")
            failures += 1
    total = (len(cases) + len(print_breaks) + len(principal_breaks) + len(notice_breaks) + len(feed17)
             + len(key_breaks) + 2)
    errors, _, _ = run_rules(base, realtime_text="nothing documented")
    if not any(e.startswith("rule 7") for e in errors):
        print("self-test: an undocumented realtime message type was not detected")
        failures += 1
    unversioned = copy.deepcopy(base)
    for _, node in walk(unversioned, []):
        if isinstance(node, dict) and "expr_version" in node and "fields" in node:
            node.pop("expr_version")
    errors, _, _ = run_rules(unversioned)
    if not any("but no expr_version" in e for e in errors):
        print("self-test: expressions without an expr_version were not detected")
        failures += 1
    print(f"self-test: {total - failures} of {total} broken specs detected")
    return 1 if failures else 0


def main() -> int:
    if "--self-test" in sys.argv[1:]:
        return self_test()
    spec = load()
    errors, n_codes, n = run_rules(spec)
    for e in errors:
        print(e)
    ops = sum(1 for _ in operations(spec))
    if errors:
        print(f"native-api: {len(errors)} problem(s)")
        return 1
    print(f"native-api {spec['info']['version']}: {ops} operations, {n_codes} error codes, "
          f"{n} examples checked, all rules pass")
    return 0


if __name__ == "__main__":
    sys.exit(main())
