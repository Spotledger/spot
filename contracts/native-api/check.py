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
18. Processes, runs and the task inbox (contract 0.12.0, WP-CON-03.T20): the fourteen operations
    exist on their method and path; only ``process_webhook`` is public (``security: []``) and it
    names its signature header and the single ``process.webhook_rejected`` answer; every other one
    needs the bearer; every new object schema is closed, every string, array and open map in them is
    bounded, no run or event schema carries variables, outputs or a payload, and the completion body
    cannot name an actor; every write names its audit, ``run_start`` and ``task_complete`` require an
    ``Idempotency-Key``, the task writes require ``If-Match``, lists are capped at 100 (events 200);
    every read names its no-oracle ``404``; the thirteen error codes are registered with their status.
19. Process versions and migrations (contract 0.13.0, WP-CON-03.T21): the seven operations exist on
    their method and path and need the bearer; every new schema is closed and bounded (the
    definition at most 256 KiB, the note at most 500); the publish body and the migration bodies
    cannot name an actor; every write says it is audited and names its idempotency behaviour
    (publish and migration start require ``Idempotency-Key``); the version reads and the migration
    operations name their permission and their no-oracle ``404``; ``process_version_get`` answers
    an ``ETag`` of a content hash; ``process_version_publish`` names ``process.publish_service_principal``
    without naming a principal; a migration to an unpublished target is ``409``; the version list is
    capped at 100; the six error codes are registered with their status; the three insight report
    names the engine registers are read from ``spot-bpm-insight`` and named by ``report_list``.
20. Graph and lineage (contract 0.14.0, WP-CON-03.T22): ``graph_query`` and ``lineage_trace`` exist
    as ``POST`` reads that need the bearer, take no ``Idempotency-Key`` and say they change nothing;
    every new schema is closed and bounded (depth 1 to 8, limit 1 to 1000, at most 16 edge types;
    lineage depth 1 to 12, limit 1 to 5000); the filter vocabulary is exactly the crate's; a node is
    ``doctype`` and ``id`` only; ``withheld`` is a flag, never a count; a lineage quantity is a
    decimal string; the answers name the no-oracle ``404 graph.not_found``, the removal of records
    the caller may not read and the truncation reasons; the five error codes are registered.
21. Webhook signing secret (contract 0.15.0, WP-CON-03.T66): ``webhook_signing_secret_put`` and
    ``webhook_signing_secret_get`` exist on their method and path and need the bearer; the secret is
    write-only (the schemas have no readable secret outside the one generated value of the ``PUT``
    answer, which is documented as once-only); the body is exactly one of a bounded secret and
    ``generate``; both operations name the Tenant Administrator, the same ``403 permission.denied``
    whether or not the webhook exists, ``Cache-Control: private, no-store`` and the audit that never
    records the value; the ``PUT`` takes no ``Idempotency-Key`` and says a repeat rotates again; no
    new error code is registered for them and every code the descriptions name is registered.
22. Journal and event selectors (contract 0.16.0, WP-CON-03.T30): ``journal_info_get``,
    ``journal_manifest_get`` and ``journal_segment_get`` exist on their paths, need the bearer and name
    the role ``journal_reader``, the Tenant Administrator, ``grant_expired`` (the same status as no
    grant) and the no-journal ``501``; the segment operation says it is the whole tenant, unfiltered,
    audited on every call, supports ``Range`` (a ``206``), answers an ``ETag`` and
    ``Cache-Control: private, no-store``; ``ConsumerSpec`` has the bounded ``classes`` (``state``,
    ``domain``, ``audit``; never telemetry) and ``entities``; ``PullBatch`` has ``served_from`` (``outbox``
    or ``journal``); ``event_consumer_pull`` and ``event_stream`` document the history rule; the three
    journal codes are registered with the statuses and retryability the contract fixes.
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
# The process, run and task lists page the same way (0.12.0): the caller's own filtered view.
PAGING_CURSOR_IN_QUERY = {("resource_list", "cursor"), ("process_list", "cursor"), ("run_list", "cursor"),
                          ("task_list", "cursor"), ("process_version_list", "cursor"), ("sso_admin_provider_list", "cursor"), ("identity_user_list", "cursor"), ("identity_invitation_list", "cursor"), ("identity_role_list", "cursor"), ("identity_assignment_list", "cursor"), ("identity_api_key_list", "cursor"), ("identity_scim_credential_list", "cursor"), ("identity_session_list", "cursor"), ("governance_grant_request_list", "cursor"), ("governance_sod_analysis", "cursor"), ("governance_sod_override_list", "cursor"), ("governance_review_list", "cursor"), ("governance_review_items", "cursor"), ("governance_question_user_permissions", "cursor"), ("governance_question_who_can", "cursor"), ("governance_question_who_can_see", "cursor"), ("governance_question_changes", "cursor"), ("governance_emergency_list", "cursor"), ("security_retention_run_list", "cursor"), ("security_retention_hold_list", "cursor"), ("security_retention_proof_list", "cursor"), ("security_protected_group_list", "cursor"), ("security_node_key_list", "cursor"), ("security_support_request_list", "cursor"), ("security_support_session_list", "cursor"), ("security_support_session_recording", "cursor"), ("security_audit_entries", "cursor"), ("security_compliance_controls", "cursor"), ("import_errors", "cursor"), ("app_list", "cursor"), ("connector_list", "cursor"), ("connector_runs", "cursor")}
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


PROCESS_OPS = {
    "process_list": ("get", "/api/v1/process"),
    "run_start": ("post", "/api/v1/process/{process}/runs"),
    "run_list": ("get", "/api/v1/process/runs"),
    "run_get": ("get", "/api/v1/process/runs/{id}"),
    "run_events": ("get", "/api/v1/process/runs/{id}/events"),
    "run_cancel": ("post", "/api/v1/process/runs/{id}/cancel"),
    "process_webhook": ("post", "/api/v1/process/hook/{trigger}"),
    "task_list": ("get", "/api/v1/task"),
    "task_get": ("get", "/api/v1/task/{id}"),
    "task_claim": ("post", "/api/v1/task/{id}/claim"),
    "task_release": ("post", "/api/v1/task/{id}/release"),
    "task_complete": ("post", "/api/v1/task/{id}/complete"),
    "task_delegate": ("post", "/api/v1/task/{id}/delegate"),
    "task_reassign": ("post", "/api/v1/task/{id}/reassign"),
}
PROCESS_CODES = {
    "process.not_found": 404, "process.input_invalid": 422, "process.run_not_found": 404,
    "process.run_not_active": 409, "process.webhook_rejected": 401, "task.not_found": 404,
    "task.already_claimed": 409, "task.not_claimed_by_caller": 409, "task.outcome_invalid": 422,
    "task.segregation": 403, "task.delegation_invalid": 422, "task.target_invalid": 422,
    "task.not_active": 409,
}
PROCESS_SCHEMAS = (
    "ProcessSummary", "ProcessPage", "RunStart", "RunCancel", "RunStep", "Run", "RunResponse", "RunPage",
    "RunEvent", "RunEventPage", "WebhookAccepted", "TaskItem", "TaskPage", "TaskOutcome", "TaskField",
    "TaskEvent", "TaskDetail", "TaskDetailResponse", "TaskResponse", "TaskComplete", "TaskDelegate",
    "TaskReassign", "DocumentRef",
)
# Open by nature (a schema, or values of a form the process defines); bounded by maxProperties.
PROCESS_OPEN_OBJECTS = {"InputSchema", "RunStart.input", "TaskComplete.values"}
NO_ORACLE = {
    "process_list": None, "process_webhook": None, "run_start": "404 process.not_found", "run_list": None,
    "run_get": "404 process.run_not_found", "run_events": "404 process.run_not_found",
    "run_cancel": "404 process.run_not_found", "task_list": None, "task_get": "404 task.not_found",
    "task_claim": "404 task.not_found", "task_release": "404 task.not_found",
    "task_complete": "404 task.not_found", "task_delegate": "404 task.not_found",
    "task_reassign": "404 task.not_found",
}
LEAKY_NAMES = re.compile(r"^(variables?|outputs?|payload|error_text|error_message|stack|trace)$", re.I)
ACTOR_NAMES = re.compile(r"actor|^by$|^user$|^principal$|on_behalf|^system$", re.I)


def _bounded(node: Any, where: str, errors: list[str], rule: int = 18) -> None:
    """Rule 18: closed objects; strings, arrays and open maps carry a bound."""
    if not isinstance(node, dict) or "$ref" in node:
        return
    types = node.get("type")
    types = types if isinstance(types, list) else [types]
    if "object" in types:
        if "properties" in node:
            if node.get("additionalProperties") is not False:
                errors.append(f"rule {rule}: {where}: an object schema here is closed (additionalProperties: false)")
            for name, sub in node["properties"].items():
                _bounded(sub, f"{where}.{name}", errors, rule)
        elif "maxProperties" not in node:
            errors.append(f"rule {rule}: {where}: an open object needs maxProperties")
    if "string" in types and not any(k in node for k in ("maxLength", "enum", "const")):
        errors.append(f"rule {rule}: {where}: a string needs maxLength (or an enum)")
    if "array" in types:
        if "maxItems" not in node:
            errors.append(f"rule {rule}: {where}: an array needs maxItems")
        _bounded(node.get("items"), f"{where}[]", errors, rule)


def check_processes_and_tasks(spec: dict[str, Any], errors: list[str]) -> None:
    """Rule 18 (contract 0.12.0, WP-CON-03.T20)."""
    paths = spec.get("paths", {})
    components = spec.get("components", {})
    schemas = components.get("schemas", {})
    registry = spec.get("x-spot-error-codes", {})
    ops: dict[str, dict[str, Any]] = {}
    for op_id, (method, route) in PROCESS_OPS.items():
        op = paths.get(route, {}).get(method)
        if not op or op.get("operationId") != op_id:
            errors.append(f"rule 18: {method.upper()} {route} ({op_id}) is missing")
            continue
        ops[op_id] = op
        text = " ".join(str(op.get("description", "")).split())
        params = list(paths[route].get("parameters", [])) + list(op.get("parameters", []))
        refs = {p.get("$ref", "").rsplit("/", 1)[-1] for p in params if "$ref" in p}
        if op_id == "process_webhook":
            if op.get("security") != []:
                errors.append("rule 18: process_webhook is the one public operation (security: [])")
            for word in ("X-Spot-Signature", "process.webhook_rejected", "raw body", "never says whether a run started"):
                if word not in text:
                    errors.append(f"rule 18: process_webhook does not state {word!r}")
            if "200" in (op.get("responses") or {}) or "202" not in (op.get("responses") or {}):
                errors.append("rule 18: process_webhook answers 202 only")
        elif "security" in op:
            errors.append(f"rule 18: {op_id} needs the bearer (no `security` override)")
        if method == "post" and op_id != "process_webhook" and "audited" not in text:
            errors.append(f"rule 18: {op_id} is a write and its description must say it is audited")
        if op_id in ("run_start", "task_complete") and "IdempotencyKeyRequired" not in refs:
            errors.append(f"rule 18: {op_id} requires an Idempotency-Key")
        if op_id.startswith("task_") and method == "post" and "IfMatch" not in refs:
            errors.append(f"rule 18: {op_id} requires If-Match")
        wanted = NO_ORACLE[op_id]
        if wanted and f"`{wanted}`" not in text:
            errors.append(f"rule 18: {op_id} must name {wanted} (the same answer as unknown)")
        if op_id in ("process_list", "run_list", "task_list", "run_events"):
            cap = 200 if op_id == "run_events" else 100
            limit = [p for p in params if p.get("name") == "limit"]
            if len(limit) != 1 or limit[0].get("schema", {}).get("maximum") != cap:
                errors.append(f"rule 18: {op_id} needs a limit parameter of at most {cap}")
        for p in params:
            if p.get("in") == "query" and re.search(r"actor|user|tenant|site", p.get("name", ""), re.I):
                errors.append(f"rule 18: {op_id}: query parameter {p['name']!r} would let a caller name someone else")
    for code, status in PROCESS_CODES.items():
        entry = registry.get(code)
        if not entry or entry[0] != status or entry[1] is not False:
            errors.append(f"rule 18: error code {code} must be registered with status {status}, not retryable")
    for name in PROCESS_SCHEMAS:
        schema = schemas.get(name)
        if schema is None:
            errors.append(f"rule 18: schema {name} is missing")
            continue
        _bounded(schema, name, errors)
        for prop in schema.get("properties", {}):
            if name in ("Run", "RunStep", "RunEvent", "TaskEvent", "TaskDetail") and LEAKY_NAMES.search(prop):
                errors.append(f"rule 18: {name}.{prop}: a run or event answer carries no variables, outputs or payload")
    for name in ("RunStart", "TaskComplete"):
        for prop in schemas.get(name, {}).get("properties", {}):
            if ACTOR_NAMES.search(prop):
                errors.append(f"rule 18: {name}.{prop}: a request cannot name an actor")
    for name, prop in (("RunStart", "input"), ("TaskComplete", "values")):
        if "maxProperties" not in schemas.get(name, {}).get("properties", {}).get(prop, {}):
            errors.append(f"rule 18: {name}.{prop} needs maxProperties")
    if "maxProperties" not in schemas.get("InputSchema", {}):
        errors.append("rule 18: InputSchema needs maxProperties")


VERSION_OPS = {
    "process_version_list": ("get", "/api/v1/process/{process}/versions"),
    "process_version_publish": ("post", "/api/v1/process/{process}/versions"),
    "process_version_get": ("get", "/api/v1/process/{process}/versions/{n}"),
    "process_version_retire": ("post", "/api/v1/process/{process}/versions/{n}/retire"),
    "process_migration_plan": ("post", "/api/v1/process/{process}/versions/{from}/migrations/plan"),
    "process_migration_start": ("post", "/api/v1/process/{process}/versions/{from}/migrations"),
    "process_migration_get": ("get", "/api/v1/process/{process}/migrations/{id}"),
}
VERSION_CODES = {
    "process.version_conflict": 409, "process.version_not_published": 409, "process.publish_denied": 403,
    "process.definition_invalid": 422, "process.migration_invalid": 422, "process.migration_not_found": 404,
}
VERSION_SCHEMAS = (
    "ProcessVersionState", "ProcessVersion", "ProcessVersionPage", "ProcessVersionResponse",
    "ProcessVersionDetail", "ProcessVersionDetailResponse", "ProcessVersionPublish", "MigrationMapping",
    "MigrationRequest", "MigrationNodeCount", "MigrationUnmoved", "MigrationPlan", "MigrationPlanResponse",
    "MigrationStarted", "MigrationIdValue", "MigrationState", "Migration", "MigrationResponse",
)
# Open by nature (values of a mapping the process defines); bounded by maxProperties.
VERSION_NO_ORACLE = {
    "process_version_list": "404 process.not_found", "process_version_publish": "404 process.not_found",
    "process_version_get": "404 process.not_found", "process_version_retire": "404 process.not_found",
    "process_migration_plan": "404 process.not_found", "process_migration_start": "404 process.not_found",
    "process_migration_get": "404 process.migration_not_found",
}
VERSION_PERMISSION = {
    "process_version_list": "process.read_versions", "process_version_publish": "process.publish",
    "process_version_get": "process.read_versions", "process_version_retire": "process.publish",
    "process_migration_plan": "process.migrate", "process_migration_start": "process.migrate",
    "process_migration_get": "process.migrate",
}
INSIGHT_REPORT_RE = re.compile(r'^pub const (?![A-Z_]*DOCTYPE)[A-Z_]+: &str = "(bpm_[a-z_]+)";', re.M)
INSIGHT_REPORT_PATH = Path(__file__).resolve().parents[2] / "crates/framework/spot-bpm-insight/src/report.rs"


def check_process_versions(spec: dict[str, Any], errors: list[str], insight_src: str | None = None) -> None:
    """Rule 19 (contract 0.13.0, WP-CON-03.T21)."""
    paths = spec.get("paths", {})
    schemas = spec.get("components", {}).get("schemas", {})
    registry = spec.get("x-spot-error-codes", {})
    for op_id, (method, route) in VERSION_OPS.items():
        op = paths.get(route, {}).get(method)
        if not op or op.get("operationId") != op_id:
            errors.append(f"rule 19: {method.upper()} {route} ({op_id}) is missing")
            continue
        text = " ".join(str(op.get("description", "")).split())
        params = list(paths[route].get("parameters", [])) + list(op.get("parameters", []))
        refs = {p.get("$ref", "").rsplit("/", 1)[-1] for p in params if "$ref" in p}
        if "security" in op:
            errors.append(f"rule 19: {op_id} needs the bearer (no `security` override)")
        if method == "post" and "audited" not in text:
            errors.append(f"rule 19: {op_id} is a write and its description must say it is audited")
        if op_id in ("process_version_publish", "process_migration_start") and "IdempotencyKeyRequired" not in refs:
            errors.append(f"rule 19: {op_id} requires an Idempotency-Key")
        if op_id in ("process_version_publish", "process_migration_start") and "Idempotency-Replayed" not in str(op):
            errors.append(f"rule 19: {op_id} must state the replay answer")
        if f"`{VERSION_PERMISSION[op_id]}`" not in text:
            errors.append(f"rule 19: {op_id} must name the permission {VERSION_PERMISSION[op_id]}")
        wanted = VERSION_NO_ORACLE[op_id]
        if f"`{wanted}`" not in text:
            errors.append(f"rule 19: {op_id} must name {wanted} (the same answer as unknown)")
        if op_id == "process_version_list":
            limit = [p for p in params if p.get("name") == "limit"]
            if len(limit) != 1 or limit[0].get("schema", {}).get("maximum") != 100:
                errors.append("rule 19: process_version_list needs a limit parameter of at most 100")
        for p in params:
            if p.get("in") == "query" and re.search(r"actor|user|tenant|site", p.get("name", ""), re.I):
                errors.append(f"rule 19: {op_id}: query parameter {p['name']!r} would let a caller name someone else")
    get = paths.get("/api/v1/process/{process}/versions/{n}", {}).get("get", {})
    if "DefinitionETag" not in str(get.get("responses", {}).get("200", {}).get("headers", {})):
        errors.append("rule 19: process_version_get must answer an ETag that is the definition's content hash")
    if "content hash" not in str(get.get("description", "")):
        errors.append("rule 19: process_version_get must say its ETag is the content hash")
    publish = " ".join(str(paths.get("/api/v1/process/{process}/versions", {}).get("post", {}).get("description", "")).split())
    for word in ("process.publish_service_principal", "403 process.publish_denied", "names no principal",
                 "409 process.version_conflict", "422 process.definition_invalid", "node ids and fault kinds only"):
        if word not in publish:
            errors.append(f"rule 19: process_version_publish does not state {word!r}")
    retire = " ".join(str(paths.get("/api/v1/process/{process}/versions/{n}/retire", {}).get("post", {}).get("description", "")).split())
    for word in ("409 process.version_not_published", "disables the process's triggers", "running runs continue"):
        if word not in retire:
            errors.append(f"rule 19: process_version_retire does not state {word!r}")
    for op_id in ("process_migration_plan", "process_migration_start"):
        route = VERSION_OPS[op_id][1]
        text = " ".join(str(paths.get(route, {}).get("post", {}).get("description", "")).split())
        if "409 process.version_not_published" not in text or "422 process.migration_invalid" not in text:
            errors.append(f"rule 19: {op_id} must state the unpublished target 409 and the invalid request 422")
    for code, status in VERSION_CODES.items():
        entry = registry.get(code)
        if not entry or entry[0] != status or entry[1] is not False:
            errors.append(f"rule 19: error code {code} must be registered with status {status}, not retryable")
    for name in VERSION_SCHEMAS:
        schema = schemas.get(name)
        if schema is None:
            errors.append(f"rule 19: schema {name} is missing")
            continue
        _bounded(schema, name, errors, 19)
    for name in ("ProcessVersionPublish", "MigrationRequest", "MigrationMapping"):
        for prop in schemas.get(name, {}).get("properties", {}):
            if ACTOR_NAMES.search(prop):
                errors.append(f"rule 19: {name}.{prop}: a request cannot name an actor")
    definition = schemas.get("ProcessVersionPublish", {}).get("properties", {}).get("definition", {})
    if definition.get("maxLength") != 262144:
        errors.append("rule 19: ProcessVersionPublish.definition is at most 256 KiB (262144)")
    note = schemas.get("ProcessVersionPublish", {}).get("properties", {}).get("note", {})
    if note.get("maxLength") != 500:
        errors.append("rule 19: ProcessVersionPublish.note is at most 500 characters")
    for name in ("nodes", "variable_defaults"):
        if "maxProperties" not in schemas.get("MigrationMapping", {}).get("properties", {}).get(name, {}):
            errors.append(f"rule 19: MigrationMapping.{name} needs maxProperties")
    for prop in schemas.get("MigrationUnmoved", {}).get("properties", {}):
        if prop not in ("run", "reason"):
            errors.append(f"rule 19: MigrationUnmoved.{prop}: an unmoved run is named by id and reason code only")
    if insight_src is None and INSIGHT_REPORT_PATH.is_file():
        insight_src = INSIGHT_REPORT_PATH.read_text(encoding="utf-8")
    if insight_src is not None:
        names = INSIGHT_REPORT_RE.findall(insight_src)
        text = " ".join(str(paths.get("/api/v1/report", {}).get("get", {}).get("description", "")).split())
        if len(names) != 3:
            errors.append(f"rule 19: spot-bpm-insight registers {len(names)} report names; the contract names three")
        for name in names:
            if f"`{name}`" not in text:
                errors.append(f"rule 19: report_list does not name the insight report {name}")
        if "never zeros" not in text:
            errors.append("rule 19: report_list must say a process the viewer may not read is absent, never zeros")


GRAPH_OPS = {
    "graph_query": "/api/v1/graph/query",
    "lineage_trace": "/api/v1/lineage",
}
GRAPH_CODES = {
    "graph.not_found": (404, False), "graph.query_invalid": (422, False), "graph.too_large": (422, False),
    "graph.unavailable": (503, True), "lineage.kind_unknown": (422, False),
}
GRAPH_SCHEMAS = (
    "GraphNodeRef", "GraphDirection", "GraphLiteral", "GraphFilter", "GraphFilterCompare", "GraphFilterIn",
    "GraphFilterNull", "GraphFilterAnd", "GraphFilterOr", "GraphFilterNot", "GraphQuery", "GraphEdgeProperties",
    "GraphEdge", "GraphTruncationReason", "GraphAnswer", "GraphAnswerResponse", "LineageKind", "LineageRequest",
    "LineageNode", "LineageLevel", "LineageTruncationReason", "LineageAnswer", "LineageAnswerResponse",
)
# The filter vocabulary of spot-graph-query/src/filter.rs: comparisons, in, null tests, and, or, not.
GRAPH_COMPARE_OPS = ["eq", "ne", "lt", "le", "gt", "ge"]
GRAPH_FILTER_FORMS = ["GraphFilterAnd", "GraphFilterCompare", "GraphFilterIn", "GraphFilterNot",
                      "GraphFilterNull", "GraphFilterOr"]


def _graph_bound(schema: dict[str, Any], prop: str, low: int, high: int) -> bool:
    p = schema.get("properties", {}).get(prop, {})
    return p.get("minimum") == low and p.get("maximum") == high


def check_graph_and_lineage(spec: dict[str, Any], errors: list[str]) -> None:
    """Rule 20 (contract 0.14.0, WP-CON-03.T22)."""
    paths = spec.get("paths", {})
    schemas = spec.get("components", {}).get("schemas", {})
    registry = spec.get("x-spot-error-codes", {})
    for op_id, route in GRAPH_OPS.items():
        op = paths.get(route, {}).get("post")
        if not op or op.get("operationId") != op_id:
            errors.append(f"rule 20: POST {route} ({op_id}) is missing")
            continue
        text = " ".join(str(op.get("description", "")).split())
        if "security" in op:
            errors.append(f"rule 20: {op_id} needs the bearer (no `security` override)")
        if op.get("parameters"):
            errors.append(f"rule 20: {op_id} is a read and takes no Idempotency-Key or other header parameter")
        for word in ("changes nothing", "`404 graph.not_found`", "`422 graph.too_large`", "`503 graph.unavailable`",
                     "removed with every path through it", "never a timeout", "Rate class `interactive`",
                     "`resource_get`", "no existence oracle"):
            if word not in text:
                errors.append(f"rule 20: {op_id} does not state {word!r}")
        if re.search(r"\bsecret\b|\btoken\b", text, re.I) and "never" not in text:
            errors.append(f"rule 20: {op_id} mentions a credential")
    graph = " ".join(str(paths.get("/api/v1/graph/query", {}).get("post", {}).get("description", "")).split())
    if "never how many" not in graph:
        errors.append("rule 20: graph_query must say `withheld` never says how many")
    lineage = " ".join(str(paths.get("/api/v1/lineage", {}).get("post", {}).get("description", "")).split())
    for word in ("`422 lineage.kind_unknown`", "exact decimal strings", "ADR-12", "cannot send one"):
        if word not in lineage:
            errors.append(f"rule 20: lineage_trace does not state {word!r}")
    for code, (status, retryable) in GRAPH_CODES.items():
        entry = registry.get(code)
        if not entry or entry[0] != status or entry[1] is not retryable:
            errors.append(f"rule 20: error code {code} must be registered with status {status}, retryable {retryable}")
    for name in GRAPH_SCHEMAS:
        schema = schemas.get(name)
        if schema is None:
            errors.append(f"rule 20: schema {name} is missing")
            continue
        _graph_closed(schema, name, errors)
    query = schemas.get("GraphQuery", {})
    if not _graph_bound(query, "depth", 1, 8):
        errors.append("rule 20: GraphQuery.depth is 1 to 8")
    if not _graph_bound(query, "limit", 1, 1000):
        errors.append("rule 20: GraphQuery.limit is 1 to 1000")
    if query.get("properties", {}).get("edge_types", {}).get("maxItems") != 16:
        errors.append("rule 20: GraphQuery.edge_types holds at most 16")
    request = schemas.get("LineageRequest", {})
    if not _graph_bound(request, "depth", 1, 12):
        errors.append("rule 20: LineageRequest.depth is 1 to 12")
    if not _graph_bound(request, "limit", 1, 5000):
        errors.append("rule 20: LineageRequest.limit is 1 to 5000")
    for name in ("GraphQuery", "LineageRequest"):
        for prop in schemas.get(name, {}).get("properties", {}):
            if ACTOR_NAMES.search(prop) or re.search(r"tenant|site|chain_config|config", prop, re.I):
                errors.append(f"rule 20: {name}.{prop}: a request cannot name an actor, a site or a chain configuration")
    forms = sorted(r.get("$ref", "").rsplit("/", 1)[-1] for r in schemas.get("GraphFilter", {}).get("oneOf", []))
    if forms != GRAPH_FILTER_FORMS:
        errors.append(f"rule 20: GraphFilter has the forms {forms}, the crate's vocabulary is {GRAPH_FILTER_FORMS}")
    compare = schemas.get("GraphFilterCompare", {}).get("properties", {}).get("op", {})
    if compare.get("enum") != GRAPH_COMPARE_OPS:
        errors.append(f"rule 20: GraphFilterCompare.op is exactly {GRAPH_COMPARE_OPS}")
    node = schemas.get("GraphNodeRef", {})
    if sorted(node.get("properties", {})) != ["doctype", "id"]:
        errors.append("rule 20: a graph node is doctype and id only")
    lnode = schemas.get("LineageNode", {})
    if sorted(lnode.get("properties", {})) != ["doctype", "id", "quantity"]:
        errors.append("rule 20: a lineage node is doctype, id and an optional quantity only")
    for where, schema, prop in (("LineageNode.quantity", lnode, "quantity"),
                                ("LineageLevel.quantity_total", schemas.get("LineageLevel", {}), "quantity_total")):
        if schema.get("properties", {}).get(prop, {}).get("$ref") != "#/components/schemas/Decimal":
            errors.append(f"rule 20: {where} is the Decimal string (ADR-12), never a number")
    for name in ("GraphAnswer", "LineageAnswer"):
        withheld = schemas.get(name, {}).get("properties", {}).get("withheld", {})
        if withheld.get("type") != "boolean":
            errors.append(f"rule 20: {name}.withheld is a boolean, never a count")
        if "withheld" not in schemas.get(name, {}).get("required", []):
            errors.append(f"rule 20: {name}.withheld is required")
    if schemas.get("GraphTruncationReason", {}).get("enum") != ["depth", "nodes", "paths"]:
        errors.append("rule 20: graph truncation reasons are depth, nodes, paths")
    if schemas.get("LineageTruncationReason", {}).get("enum") != ["depth", "nodes"]:
        errors.append("rule 20: lineage truncation reasons are depth, nodes")
    if schemas.get("LineageKind", {}).get("enum") != ["chain", "trace_forward", "trace_back", "bom", "where_used"]:
        errors.append("rule 20: LineageKind is chain, trace_forward, trace_back, bom, where_used")


def _graph_closed(node: Any, where: str, errors: list[str], rule: int = 20) -> None:
    """Rule 20 and 21: closed objects; strings, arrays and open maps carry a bound (references are followed by name)."""
    if not isinstance(node, dict) or "$ref" in node:
        return
    types = node.get("type")
    types = types if isinstance(types, list) else [types]
    if "object" in types:
        if "properties" in node:
            if node.get("additionalProperties") is not False:
                errors.append(f"rule {rule}: {where}: an object schema here is closed (additionalProperties: false)")
            for name, sub in node["properties"].items():
                _graph_closed(sub, f"{where}.{name}", errors, rule)
        elif "maxProperties" not in node:
            errors.append(f"rule {rule}: {where}: an open object needs maxProperties")
        else:
            _graph_closed(node.get("additionalProperties"), f"{where}.*", errors, rule)
    if "string" in types and not any(k in node for k in ("maxLength", "enum", "const")):
        errors.append(f"rule {rule}: {where}: a string needs maxLength (or an enum)")
    if "array" in types:
        if "maxItems" not in node:
            errors.append(f"rule {rule}: {where}: an array needs maxItems")
        _graph_closed(node.get("items"), f"{where}[]", errors, rule)
    for key in ("oneOf", "allOf"):
        for i, sub in enumerate(node.get(key, [])):
            _graph_closed(sub, f"{where}.{key}[{i}]", errors, rule)


SIGNING_SECRET_ROUTE = "/api/v1/webhooks/{name}/signing-secret"


def check_signing_secret(spec: dict[str, Any], errors: list[str]) -> None:
    """Rule 21 (contract 0.15.0, WP-CON-03.T66)."""
    paths = spec.get("paths", {})
    schemas = spec.get("components", {}).get("schemas", {})
    route = paths.get(SIGNING_SECRET_ROUTE, {})
    for method, op_id in (("put", "webhook_signing_secret_put"), ("get", "webhook_signing_secret_get")):
        op = route.get(method)
        if not op or op.get("operationId") != op_id:
            errors.append(f"rule 21: {method.upper()} {SIGNING_SECRET_ROUTE} ({op_id}) is missing")
            continue
        text = " ".join(str(op.get("description", "")).split())
        if "security" in op:
            errors.append(f"rule 21: {op_id} needs the bearer (no `security` override)")
        for word in ("Tenant Administrator", "`403 permission.denied`", "same answer whether or not the webhook exists",
                     "`404 document.not_found`", "`503 server.unavailable`", "private, no-store",
                     "`service.not_configured` with status 501", "never"):
            if word not in text:
                errors.append(f"rule 21: {op_id} does not state {word!r}")
        ok_header = op.get("responses", {}).get("200", {}).get("headers", {}).get("Cache-Control", {})
        if ok_header.get("schema", {}).get("const") != "private, no-store":
            errors.append(f"rule 21: {op_id} must answer Cache-Control: private, no-store")
        if any(p.get("in") == "query" for p in route.get("parameters", []) + op.get("parameters", [])):
            errors.append(f"rule 21: {op_id}: a secret or a name never rides in a query string")
    put = " ".join(str(route.get("put", {}).get("description", "")).split())
    for word in ("audited", "never contains the value", "a repeat rotates the secret again", "`webhook_secret.too_short`",
                 "`webhook_secret.too_long`", "`webhook_secret.bad_request`", "in this one response", "never echoed"):
        if word not in put:
            errors.append(f"rule 21: webhook_signing_secret_put does not state {word!r}")
    if "Idempotency" in str(route.get("put", {}).get("parameters", "")):
        errors.append("rule 21: webhook_signing_secret_put takes no Idempotency-Key (a repeat rotates again)")
    get = " ".join(str(route.get("get", {}).get("description", "")).split())
    if "The value is never returned" not in get:
        errors.append("rule 21: webhook_signing_secret_get must say the value is never returned")
    body = schemas.get("WebhookSigningSecretPut", {})
    # One object with two optional properties and a required-only oneOf. A oneOf of two inline objects makes
    # Kiota emit a clashing composed wrapper in the request builder (0.26.1), so the union is not spelled that way.
    if body.get("type") != "object" or sorted(body.get("properties", {})) != ["generate", "secret"]:
        errors.append("rule 21: WebhookSigningSecretPut is one object with the properties secret and generate")
    if body.get("additionalProperties") is not False:
        errors.append("rule 21: WebhookSigningSecretPut is closed (additionalProperties: false)")
    if sorted(tuple(b.get("required", [])) for b in body.get("oneOf", [])) != [("generate",), ("secret",)] \
            or any(set(b) != {"required"} for b in body.get("oneOf", [])):
        errors.append("rule 21: WebhookSigningSecretPut is exactly one of secret and generate (a required-only oneOf)")
    sec = body.get("properties", {}).get("secret")
    if sec is not None and (sec.get("minLength") != 16 or sec.get("maxLength") != 256):
        errors.append("rule 21: a supplied signing secret is bounded to 16 to 256")
    for name in ("WebhookSigningSecretPutResponse", "WebhookSigningSecretStateResponse"):
        schema = schemas.get(name)
        if schema is None:
            errors.append(f"rule 21: schema {name} is missing")
            continue
        _graph_closed(schema, name, errors, 21)
        data = schema.get("properties", {}).get("data", {})
        if data.get("additionalProperties") is not False:
            errors.append(f"rule 21: {name}.data is closed")
        for prop in data.get("properties", {}):
            if prop not in ("key_version", "fingerprint", "set", "secret"):
                errors.append(f"rule 21: {name}.data.{prop}: only key_version, fingerprint, set (and a generated secret)")
    if "secret" in schemas.get("WebhookSigningSecretStateResponse", {}).get("properties", {}).get("data", {}).get("properties", {}):
        errors.append("rule 21: the GET answer never carries the secret")
    if "once" not in str(schemas.get("WebhookSigningSecretPutResponse", {}).get("properties", {}).get("data", {}).get(
            "properties", {}).get("secret", {}).get("description", "")) and "only in this response" not in str(
            schemas.get("WebhookSigningSecretPutResponse", {})):
        errors.append("rule 21: the generated secret of the PUT answer is documented as shown once")
    fp = schemas.get("WebhookSigningSecretPutResponse", {}).get("properties", {}).get("data", {}).get(
        "properties", {}).get("fingerprint", {})
    if fp.get("maxLength") != 12:
        errors.append("rule 21: the fingerprint is twelve characters, never the value")
    registry = spec.get("x-spot-error-codes", {})
    for code in ("permission.denied", "document.not_found", "api.invalid_request", "server.unavailable", "auth.required"):
        if code not in registry:
            errors.append(f"rule 21: error code {code} must stay registered")
    if any(code.startswith("webhook_secret.") for code in registry):
        errors.append("rule 21: the webhook_secret.* catalogue codes ride in details.reason and are not registry codes")

JOURNAL_ROUTES = {
    "journal_info_get": "/api/v1/journal",
    "journal_manifest_get": "/api/v1/journal/manifest/{generation}",
    "journal_segment_get": "/api/v1/journal/segments/{generation}/{first_seq}-{last_seq}",
}


def check_journal_and_selectors(spec: dict[str, Any], errors: list[str]) -> None:
    """Rule 22 (contract 0.16.0, WP-CON-03.T30)."""
    paths = spec.get("paths", {})
    schemas = spec.get("components", {}).get("schemas", {})
    for op_id, route in JOURNAL_ROUTES.items():
        op = paths.get(route, {}).get("get")
        if not op or op.get("operationId") != op_id:
            errors.append(f"rule 22: GET {route} ({op_id}) is missing")
            continue
        text = " ".join(str(op.get("description", "")).split())
        if "security" in op:
            errors.append(f"rule 22: {op_id} needs the bearer (no `security` override)")
        for word in ("journal_reader", "Tenant Administrator", "`403 permission.denied`", "grant_expired",
                     "`service.not_configured` with status 501", "audit record", "private, no-store"):
            if word not in text and word not in text.lower():
                errors.append(f"rule 22: {op_id} does not state {word!r}")
        ok = op.get("responses", {}).get("200", {}).get("headers", {}).get("Cache-Control", {})
        if ok.get("schema", {}).get("const") != "private, no-store":
            errors.append(f"rule 22: {op_id} must answer Cache-Control: private, no-store")
    seg = paths.get(JOURNAL_ROUTES["journal_segment_get"], {}).get("get", {})
    seg_text = " ".join(str(seg.get("description", "")).split())
    for word in ("WHOLE TENANT", "UNFILTERED", "EVERY CALL IS AN AUDIT RECORD", "`Range`", "`ETag`",
                 "`404 journal.range_unavailable`", "tail chunk"):
        if word not in seg_text:
            errors.append(f"rule 22: journal_segment_get does not state {word!r}")
    if "206" not in seg.get("responses", {}):
        errors.append("rule 22: journal_segment_get supports Range and so answers 206")
    if "ETag" not in seg.get("responses", {}).get("200", {}).get("headers", {}):
        errors.append("rule 22: journal_segment_get answers an ETag")
    for op_id, route in JOURNAL_ROUTES.items():
        for param in paths.get(route, {}).get("parameters", []):
            if param.get("in") != "path":
                errors.append(f"rule 22: {op_id}: journal parameters ride in the path, never a query string")
    spec_schema = schemas.get("ConsumerSpec", {}).get("properties", {})
    classes = spec_schema.get("classes", {})
    if classes.get("items", {}).get("enum") != ["state", "domain", "audit"]:
        errors.append("rule 22: ConsumerSpec.classes is exactly state, domain, audit (telemetry is never available)")
    if classes.get("maxItems") != 3 or classes.get("minItems") != 1 or classes.get("uniqueItems") is not True:
        errors.append("rule 22: ConsumerSpec.classes is bounded (1 to 3, unique)")
    entities = spec_schema.get("entities", {})
    if entities.get("maxItems") is None or entities.get("items", {}).get("maxLength") is None:
        errors.append("rule 22: ConsumerSpec.entities is bounded")
    if "classes" in schemas.get("ConsumerSpec", {}).get("required", []):
        errors.append("rule 22: ConsumerSpec.classes is optional (a 0.15.0 client sends none)")
    served = schemas.get("PullBatch", {}).get("properties", {}).get("served_from", {})
    if served.get("enum") != ["outbox", "journal"]:
        errors.append("rule 22: PullBatch.served_from is outbox or journal")
    if "served_from" in schemas.get("PullBatch", {}).get("required", []):
        errors.append("rule 22: PullBatch.served_from is optional on the wire (a client ignores unknown fields)")
    for op_id in ("event_consumer_pull", "event_stream"):
        op = next((o for item in paths.values() for o in item.values()
                   if isinstance(o, dict) and o.get("operationId") == op_id), None)
        text = " ".join(str((op or {}).get("description", "")).split())
        for word in ("journal", "outbox", "eventstream.resync_required"):
            if word not in text:
                errors.append(f"rule 22: {op_id} does not document the history rule ({word!r})")
    registry = spec.get("x-spot-error-codes", {})
    for code, status, retry in (("journal.range_unavailable", 404, False), ("journal.commit_outcome_unknown", 503, True),
                                ("journal.halted", 503, True)):
        row = registry.get(code)
        if not row or row[0] != status or row[1] is not retry:
            errors.append(f"rule 22: error code {code} must be registered as [{status}, {retry}]")
    for code in ("permission.denied", "eventstream.resync_required", "server.unavailable"):
        if code not in registry:
            errors.append(f"rule 22: error code {code} must stay registered")

# ---- table-driven rules for the operation groups of 0.17.0 and later (one pin per group) ----------
GROUP_PINS: dict[int, dict[str, Any]] = {}
SECRET_NAMES = {"secret", "client_secret", "password", "token", "private_key", "signature", "api_key", "key_material",
                "recovery_codes", "provisioning_secret", "invitation_token", "key"}


def _walk_props(spec: dict[str, Any], node: Any, seen: set[str], where: str) -> Iterator[tuple[str, str]]:
    """Yields (property name, where) for every property reachable from ``node``, following references once."""
    if not isinstance(node, dict):
        return
    if "$ref" in node:
        name = node["$ref"].rsplit("/", 1)[-1]
        if name in seen:
            return
        seen.add(name)
        yield from _walk_props(spec, spec.get("components", {}).get("schemas", {}).get(name), seen, name)
        return
    for prop, sub in node.get("properties", {}).items():
        yield prop, where
        yield from _walk_props(spec, sub, seen, f"{where}.{prop}")
    for key in ("items", "additionalProperties"):
        yield from _walk_props(spec, node.get(key), seen, where)
    for key in ("oneOf", "allOf", "anyOf"):
        for sub in node.get(key, []):
            yield from _walk_props(spec, sub, seen, where)


def check_group(rule: int, pin: dict[str, Any], spec: dict[str, Any], errors: list[str]) -> None:
    """One operation group: the operations exist where the pin says, with the stated security text."""
    paths = spec.get("paths", {})
    schemas = spec.get("components", {}).get("schemas", {})
    registry = spec.get("x-spot-error-codes", {})
    r = f"rule {rule}"
    for op_id, (method, route, public, step_up, idem_required) in pin["ops"].items():
        op = paths.get(route, {}).get(method)
        if not op or op.get("operationId") != op_id:
            errors.append(f"{r}: {method.upper()} {route} ({op_id}) is missing")
            continue
        text = " ".join(str(op.get("description", "")).split())
        if public and op.get("security") != []:
            errors.append(f"{r}: {op_id} is public and says so (`security: []`)")
        if not public and "security" in op:
            errors.append(f"{r}: {op_id} needs the bearer (no `security` override)")
        words = ["Permission:", "Rate class", "Audience:"]
        if method != "get":
            words.append("Audited:")
        if step_up:
            words.append("`403 auth.step_up_required`")
        if not public and "consumer (ADR-083)" not in text:
            words.append("never the consumer audience")
        for word in words:
            if word not in text:
                errors.append(f"{r}: {op_id} does not state {word!r}")
        params = op.get("parameters", [])
        refs = {p.get("$ref", "").rsplit("/", 1)[-1] for p in params}
        if idem_required and "IdempotencyKeyRequired" not in refs:
            errors.append(f"{r}: {op_id} requires an Idempotency-Key")
        for p in params + paths.get(route, {}).get("parameters", []):
            if p.get("in") == "query" and re.search(r"token|secret|password|key", str(p.get("name", "")), re.I):
                errors.append(f"{r}: {op_id}: query parameter {p.get('name')!r} is named like a secret")
        if "default" not in op.get("responses", {}):
            errors.append(f"{r}: {op_id} has no default error response")
        once = op_id in pin.get("once", [])
        for status, resp in op.get("responses", {}).items():
            if status == "default":
                continue
            node = resp.get("content", {}).get("application/json", {}).get("schema")
            for prop, where in _walk_props(spec, node, set(), op_id):
                if prop in SECRET_NAMES and not once:
                    errors.append(f"{r}: {op_id}: response property {prop!r} (in {where}): only a shown-once operation returns a secret")
                if re.search(r"(storage|db|database)_?key", prop):
                    errors.append(f"{r}: {op_id}: response property {prop!r} names a storage key")
        if once and "shown once" not in text:
            errors.append(f"{r}: {op_id} returns a secret and must say it is shown once")
    for name in pin["schemas"]:
        schema = schemas.get(name)
        if schema is None:
            errors.append(f"{r}: schema {name} is missing")
            continue
        _graph_closed(schema, name, errors, rule)
    for code, (status, retry) in pin["codes"].items():
        row = registry.get(code)
        if not row or row[0] != status or row[1] is not retry:
            errors.append(f"{r}: error code {code} must be registered as [{status}, {retry}]")
    for code in ("permission.denied", "document.not_found", "api.invalid_request", "auth.required"):
        if code not in registry:
            errors.append(f"{r}: error code {code} must stay registered")



# Rule 23 (contract 0.17.0, WP-CON-03.T40): single sign-on (OIDC and SAML 2.0) and the provider and sign-in policy administration, plus step-up.
GROUP_PINS[23] = {'version': '0.17.0',
 'task': 'WP-CON-03.T40',
 'ops': {'sso_provider_list': ('get', '/api/v1/auth/sso/providers', True, False, False),
         'sso_begin': ('post', '/api/v1/auth/sso/{provider}/begin', True, False, False),
         'sso_callback': ('get', '/api/v1/auth/sso/{provider}/callback', True, False, False),
         'sso_acs': ('post', '/api/v1/auth/sso/{provider}/acs', True, False, False),
         'sso_complete': ('post', '/api/v1/auth/sso/complete', True, False, False),
         'sso_backchannel_logout': ('post', '/api/v1/auth/sso/{provider}/backchannel-logout', True, False, False),
         'sso_slo_get': ('get', '/api/v1/auth/sso/{provider}/slo', True, False, False),
         'sso_slo_post': ('post', '/api/v1/auth/sso/{provider}/slo', True, False, False),
         'sso_saml_metadata': ('get', '/api/v1/auth/sso/{provider}/saml-metadata', True, False, False),
         'auth_step_up': ('post', '/api/v1/auth/step-up', False, False, False),
         'sso_admin_provider_list': ('get', '/api/v1/identity/sso/providers', False, False, False),
         'sso_admin_provider_create': ('post', '/api/v1/identity/sso/providers', False, True, True),
         'sso_admin_provider_get': ('get', '/api/v1/identity/sso/providers/{id}', False, False, False),
         'sso_admin_provider_put': ('put', '/api/v1/identity/sso/providers/{id}', False, True, False),
         'sso_admin_provider_delete': ('delete', '/api/v1/identity/sso/providers/{id}', False, True, False),
         'sso_admin_provider_test': ('post', '/api/v1/identity/sso/providers/{id}/test', False, True, False),
         'sso_admin_policy_get': ('get', '/api/v1/identity/sso/policy', False, False, False),
         'sso_admin_policy_put': ('put', '/api/v1/identity/sso/policy', False, True, False)},
 'once': ['sso_complete'],
 'codes': {'auth.step_up_required': (403, False),
           'sso.required': (403, False),
           'sso.sign_in_failed': (401, False),
           'sso.not_allowed': (403, False),
           'sso.state_invalid': (400, False),
           'sso.request_invalid': (400, False),
           'sso.provider_not_found': (404, False),
           'sso.provider_unavailable': (503, True),
           'sso.logout_rejected': (400, False),
           'sso.provider_invalid': (422, False),
           'sso.would_lock_out': (409, False)},
 'schemas': ['AuthStepUpRequest',
             'AuthStepUpResponse',
             'SsoAcsRequest',
             'SsoAdminPolicyGetResponse',
             'SsoAdminPolicyPutRequest',
             'SsoAdminPolicyPutResponse',
             'SsoAdminProviderCreateRequest',
             'SsoAdminProviderCreateResponse',
             'SsoAdminProviderGetResponse',
             'SsoAdminProviderListResponse',
             'SsoAdminProviderPutRequest',
             'SsoAdminProviderPutResponse',
             'SsoAdminProviderTestResponse',
             'SsoBackchannelLogoutRequest',
             'SsoBeginRequest',
             'SsoBeginResponse',
             'SsoCompleteResponse',
             'SsoPolicyInput',
             'SsoProviderInput',
             'SsoProviderListResponse',
             'SsoProviderPublic',
             'SsoProviderTestResult',
             'SsoProviderView',
             'SsoSamlMetadataResponse',
             'SsoSloPostRequest']}


def check_group_extra(rule: int, pin: dict[str, Any], spec: dict[str, Any], errors: list[str]) -> None:
    """Group-specific invariants carried by the pin: routes that never take DELETE; prefixes whose writes need step-up."""
    paths = spec.get("paths", {})
    for route in pin.get("no_delete", []):
        if "delete" in paths.get(route, {}):
            errors.append(f"rule {rule}: {route} takes no DELETE")
    for prefix, exceptions in pin.get("writes_need_step_up", []):
        for route, item in paths.items():
            if not route.startswith(prefix):
                continue
            for method, op in item.items():
                if method in ("post", "put", "patch", "delete") and op.get("operationId") not in exceptions:
                    if "`403 auth.step_up_required`" not in " ".join(str(op.get("description", "")).split()):
                        errors.append(f"rule {rule}: {op.get('operationId')} is a write under {prefix} and needs a step-up")
    schemas = spec.get("components", {}).get("schemas", {})
    for op_id, names in pin.get("forbid_body_props", {}).items():
        op = next((o for item in paths.values() for o in item.values() if isinstance(o, dict) and o.get("operationId") == op_id), None)
        body = ((op or {}).get("requestBody", {}).get("content", {}).get("application/json", {}).get("schema", {}))
        node = schemas.get(body.get("$ref", "").rsplit("/", 1)[-1], body)
        for prop in node.get("properties", {}):
            if prop in names:
                errors.append(f"rule {rule}: {op_id} takes no {prop!r} (the actor is the authenticated user)")
    for op_id, names in pin.get("require_query", {}).items():
        op = next((o for item in paths.values() for o in item.values() if isinstance(o, dict) and o.get("operationId") == op_id), None)
        have = {p.get("name") for p in (op or {}).get("parameters", [])}
        for name in names:
            if name not in have:
                errors.append(f"rule {rule}: {op_id} has no {name!r} parameter")
    for op_id, prop in pin.get("body_requires_when", {}).items():
        op = next((o for item in paths.values() for o in item.values() if isinstance(o, dict) and o.get("operationId") == op_id), None)
        body = ((op or {}).get("requestBody", {}).get("content", {}).get("application/json", {}).get("schema", {}))
        node = schemas.get(body.get("$ref", "").rsplit("/", 1)[-1], body)
        real = [b for b in node.get("oneOf", []) if b.get("properties", {}).get("dry_run", {}).get("const") is False]
        if not real or prop not in real[0].get("required", []):
            errors.append(f"rule {rule}: {op_id}: a real run (dry_run false) requires {prop!r}")
    for name, props in pin.get("schema_props", {}).items():
        for prop in props:
            if prop not in schemas.get(name, {}).get("properties", {}):
                errors.append(f"rule {rule}: schema {name} has no property {prop!r}")
    for name, path, want in pin.get("schema_values", []):
        node: Any = schemas.get(name, {})
        for part in path:
            node = node.get(part, {}) if isinstance(node, dict) else {}
        if node != want:
            errors.append(f"rule {rule}: {name}.{'.'.join(path)} must be {want!r}")
    for op_id, words in pin.get("op_words", {}).items():
        op = next((o for item in paths.values() for o in item.values() if isinstance(o, dict) and o.get("operationId") == op_id), None)
        text = " ".join(str((op or {}).get("description", "")).split())
        for word in words:
            if word not in text:
                errors.append(f"rule {rule}: {op_id} does not state {word!r}")
    for doc, words in pin.get("doc_words", {}).items():
        path = Path(__file__).resolve().parent / doc
        text = path.read_text(encoding="utf-8") if path.exists() else ""
        if not text:
            errors.append(f"rule {rule}: {doc} is missing")
        for word in words:
            if word not in text:
                errors.append(f"rule {rule}: {doc} does not state {word!r}")


# Rule 24 (contract 0.18.0, WP-CON-03.T41): identity administration (users, invitations, roles, assignments, assignment rules, API keys, SCIM credentials, sessions, impersonation) and TOTP enrolment.
GROUP_PINS[24] = {'version': '0.18.0',
 'task': 'WP-CON-03.T41',
 'ops': {'identity_user_list': ('get', '/api/v1/identity/users', False, False, False),
         'identity_user_create': ('post', '/api/v1/identity/users', False, True, True),
         'identity_user_get': ('get', '/api/v1/identity/users/{id}', False, False, False),
         'identity_user_patch': ('patch', '/api/v1/identity/users/{id}', False, True, False),
         'identity_user_disable': ('post', '/api/v1/identity/users/{id}/disable', False, True, False),
         'identity_user_enable': ('post', '/api/v1/identity/users/{id}/enable', False, True, False),
         'identity_user_anonymise': ('post', '/api/v1/identity/users/{id}/anonymise', False, True, False),
         'identity_user_recover': ('post', '/api/v1/identity/users/{id}/recover', False, True, False),
         'identity_invitation_list': ('get', '/api/v1/identity/invitations', False, False, False),
         'identity_invitation_revoke': ('delete', '/api/v1/identity/invitations/{id}', False, True, False),
         'identity_invitation_resend': ('post', '/api/v1/identity/invitations/{id}/resend', False, True, False),
         'auth_invitation_accept': ('post', '/api/v1/auth/invitations/accept', True, False, False),
         'identity_role_list': ('get', '/api/v1/identity/roles', False, False, False),
         'identity_role_get': ('get', '/api/v1/identity/roles/{id}', False, False, False),
         'identity_role_customise': ('post', '/api/v1/identity/roles/{id}/customise', False, True, False),
         'identity_role_upgrade_diff': ('get', '/api/v1/identity/roles/{id}/upgrade-diff', False, False, False),
         'identity_assignment_list': ('get', '/api/v1/identity/assignments', False, False, False),
         'identity_assignment_create': ('post', '/api/v1/identity/assignments', False, True, True),
         'identity_assignment_delete': ('delete', '/api/v1/identity/assignments/{id}', False, True, False),
         'identity_assignment_rules_get': ('get', '/api/v1/identity/assignment-rules', False, False, False),
         'identity_assignment_rules_put': ('put', '/api/v1/identity/assignment-rules', False, True, False),
         'identity_assignment_rules_preview': ('post',
                                               '/api/v1/identity/assignment-rules/preview',
                                               False,
                                               False,
                                               False),
         'identity_assignment_rules_run': ('post', '/api/v1/identity/assignment-rules/run', False, True, True),
         'identity_api_key_list': ('get', '/api/v1/identity/api-keys', False, False, False),
         'identity_api_key_create': ('post', '/api/v1/identity/api-keys', False, True, True),
         'identity_api_key_delete': ('delete', '/api/v1/identity/api-keys/{id}', False, True, False),
         'identity_scim_credential_list': ('get', '/api/v1/identity/scim-credentials', False, False, False),
         'identity_scim_credential_create': ('post', '/api/v1/identity/scim-credentials', False, True, True),
         'identity_scim_credential_delete': ('delete', '/api/v1/identity/scim-credentials/{id}', False, True, False),
         'identity_session_list': ('get', '/api/v1/identity/sessions', False, False, False),
         'identity_session_end': ('delete', '/api/v1/identity/sessions/{id}', False, True, False),
         'identity_user_sessions_end_all': ('post',
                                            '/api/v1/identity/users/{id}/sessions/end-all',
                                            False,
                                            True,
                                            False),
         'identity_impersonation_start': ('post', '/api/v1/identity/impersonation', False, True, False),
         'identity_impersonation_end': ('delete', '/api/v1/identity/impersonation', False, False, False),
         'auth_totp_enrol': ('post', '/api/v1/auth/totp/enrol', False, False, False),
         'auth_totp_confirm': ('post', '/api/v1/auth/totp/confirm', False, False, False),
         'auth_totp_disable': ('post', '/api/v1/auth/totp/disable', False, False, False),
         'auth_recovery_codes_regenerate': ('post', '/api/v1/auth/recovery-codes/regenerate', False, False, False)},
 'once': ['identity_api_key_create',
          'identity_scim_credential_create',
          'auth_totp_enrol',
          'auth_totp_confirm',
          'auth_recovery_codes_regenerate'],
 'codes': {'identity.not_found': (404, False),
           'identity.invalid': (422, False),
           'identity.user_exists': (409, False),
           'identity.last_administrator': (409, False),
           'identity.sod_conflict': (409, False),
           'identity.scope_exceeds_owner': (422, False),
           'identity.invitation_invalid': (400, False),
           'identity.impersonation_refused': (403, False),
           'identity.totp_state': (409, False),
           'identity.rules_invalid': (422, False)},
 'schemas': ['AuthInvitationAcceptRequest',
             'AuthInvitationAcceptResponse',
             'AuthRecoveryCodesRegenerateRequest',
             'AuthRecoveryCodesRegenerateResponse',
             'AuthTotpConfirmRequest',
             'AuthTotpConfirmResponse',
             'AuthTotpDisableRequest',
             'AuthTotpEnrolResponse',
             'IdentityApiKey',
             'IdentityApiKeyCreateRequest',
             'IdentityApiKeyCreateResponse',
             'IdentityApiKeyListResponse',
             'IdentityAssignment',
             'IdentityAssignmentCreateRequest',
             'IdentityAssignmentCreateResponse',
             'IdentityAssignmentListResponse',
             'IdentityAssignmentRules',
             'IdentityAssignmentRulesGetResponse',
             'IdentityAssignmentRulesPreviewRequest',
             'IdentityAssignmentRulesPreviewResponse',
             'IdentityAssignmentRulesPutRequest',
             'IdentityAssignmentRulesPutResponse',
             'IdentityAssignmentRulesRunRequest',
             'IdentityAssignmentRulesRunResponse',
             'IdentityImpersonationStartRequest',
             'IdentityImpersonationStartResponse',
             'IdentityInvitation',
             'IdentityInvitationListResponse',
             'IdentityRole',
             'IdentityRoleCustomiseRequest',
             'IdentityRoleCustomiseResponse',
             'IdentityRoleGetResponse',
             'IdentityRoleListResponse',
             'IdentityRoleUpgradeDiffResponse',
             'IdentityScimCredential',
             'IdentityScimCredentialCreateResponse',
             'IdentityScimCredentialListResponse',
             'IdentitySession',
             'IdentitySessionListResponse',
             'IdentityUser',
             'IdentityUserAnonymiseRequest',
             'IdentityUserAnonymiseResponse',
             'IdentityUserCreateRequest',
             'IdentityUserCreateResponse',
             'IdentityUserDisableResponse',
             'IdentityUserEnableResponse',
             'IdentityUserGetResponse',
             'IdentityUserListResponse',
             'IdentityUserPatchRequest',
             'IdentityUserPatchResponse',
             'IdentityUserRecoverRequest',
             'IdentityUserSessionsEndAllResponse'],
 'no_delete': ['/api/v1/identity/users/{id}'],
 'writes_need_step_up': [('/api/v1/identity/', ['identity_impersonation_end', 'identity_assignment_rules_preview'])]}

# Rule 25 (contract 0.19.0, WP-CON-03.T42): access governance (grant requests, segregation of duties, access reviews, the four questions, emergency access).
GROUP_PINS[25] = {'version': '0.19.0',
 'task': 'WP-CON-03.T42',
 'ops': {'governance_grant_request_list': ('get', '/api/v1/governance/grant-requests', False, False, False),
         'governance_grant_request_create': ('post', '/api/v1/governance/grant-requests', False, False, True),
         'governance_grant_request_decide': ('post',
                                             '/api/v1/governance/grant-requests/{id}/decide',
                                             False,
                                             True,
                                             True),
         'governance_sod_rules_get': ('get', '/api/v1/governance/sod/rules', False, False, False),
         'governance_sod_rules_put': ('put', '/api/v1/governance/sod/rules', False, True, False),
         'governance_sod_analysis': ('get', '/api/v1/governance/sod/analysis', False, False, False),
         'governance_sod_override_list': ('get', '/api/v1/governance/sod/overrides', False, False, False),
         'governance_sod_override_create': ('post', '/api/v1/governance/sod/overrides', False, True, True),
         'governance_review_list': ('get', '/api/v1/governance/access-reviews', False, False, False),
         'governance_review_create': ('post', '/api/v1/governance/access-reviews', False, True, True),
         'governance_review_get': ('get', '/api/v1/governance/access-reviews/{id}', False, False, False),
         'governance_review_items': ('get', '/api/v1/governance/access-reviews/{id}/items', False, False, False),
         'governance_review_item_decide': ('post',
                                           '/api/v1/governance/access-reviews/{id}/items/{item}/decide',
                                           False,
                                           True,
                                           True),
         'governance_review_close': ('post', '/api/v1/governance/access-reviews/{id}/close', False, True, False),
         'governance_review_report': ('get', '/api/v1/governance/access-reviews/{id}/report', False, False, False),
         'governance_question_user_permissions': ('get',
                                                  '/api/v1/governance/questions/user-permissions',
                                                  False,
                                                  False,
                                                  False),
         'governance_question_who_can': ('get', '/api/v1/governance/questions/who-can', False, False, False),
         'governance_question_who_can_see': ('get', '/api/v1/governance/questions/who-can-see', False, False, False),
         'governance_question_changes': ('get', '/api/v1/governance/questions/changes', False, False, False),
         'governance_emergency_create': ('post', '/api/v1/governance/emergency-access', False, True, True),
         'governance_emergency_list': ('get', '/api/v1/governance/emergency-access', False, False, False),
         'governance_emergency_review': ('post',
                                         '/api/v1/governance/emergency-access/{id}/review',
                                         False,
                                         True,
                                         False)},
 'once': [],
 'codes': {'governance.not_found': (404, False),
           'governance.invalid': (422, False),
           'governance.self_approval': (403, False),
           'governance.request_not_pending': (409, False),
           'governance.override_invalid': (422, False),
           'governance.review_closed': (409, False),
           'governance.not_reviewer': (403, False),
           'governance.controller_is_beneficiary': (403, False)},
 'schemas': ['GovernanceEmergency',
             'GovernanceEmergencyCreateRequest',
             'GovernanceEmergencyCreateResponse',
             'GovernanceEmergencyListResponse',
             'GovernanceEmergencyReviewRequest',
             'GovernanceEmergencyReviewResponse',
             'GovernanceGrantRequest',
             'GovernanceGrantRequestCreateRequest',
             'GovernanceGrantRequestCreateResponse',
             'GovernanceGrantRequestDecideRequest',
             'GovernanceGrantRequestDecideResponse',
             'GovernanceGrantRequestListResponse',
             'GovernanceQuestionChangesResponse',
             'GovernanceQuestionUserPermissionsResponse',
             'GovernanceQuestionWhoCanResponse',
             'GovernanceQuestionWhoCanSeeResponse',
             'GovernanceReview',
             'GovernanceReviewCloseResponse',
             'GovernanceReviewCreateRequest',
             'GovernanceReviewCreateResponse',
             'GovernanceReviewGetResponse',
             'GovernanceReviewItem',
             'GovernanceReviewItemDecideRequest',
             'GovernanceReviewItemDecideResponse',
             'GovernanceReviewItemsResponse',
             'GovernanceReviewListResponse',
             'GovernanceReviewReportResponse',
             'GovernanceSodAnalysisResponse',
             'GovernanceSodOverride',
             'GovernanceSodOverrideCreateRequest',
             'GovernanceSodOverrideCreateResponse',
             'GovernanceSodOverrideListResponse',
             'GovernanceSodRules',
             'GovernanceSodRulesGetResponse',
             'GovernanceSodRulesPutRequest',
             'GovernanceSodRulesPutResponse'],
 'forbid_body_props': {'governance_grant_request_decide': ['approver', 'approver_id', 'decided_by', 'decider'],
                       'governance_review_item_decide': ['reviewer', 'reviewer_id', 'decided_by'],
                       'governance_emergency_review': ['controller', 'controller_id']},
 'require_query': {'governance_question_user_permissions': ['user', 'cursor', 'limit']}}


# Rule 26 (contract 0.20.0, WP-CON-03.T43): security administration (data policy and retention, protected groups and node keys, support access consent, audit chain and compliance evidence).
GROUP_PINS[26] = {'version': '0.20.0',
 'task': 'WP-CON-03.T43',
 'ops': {'security_data_policy_get': ('get', '/api/v1/security/data-policy', False, False, False),
         'security_data_policy_put': ('put', '/api/v1/security/data-policy', False, True, False),
         'security_retention_run_list': ('get', '/api/v1/security/retention/runs', False, False, False),
         'security_retention_run_create': ('post', '/api/v1/security/retention/runs', False, True, True),
         'security_retention_hold_list': ('get', '/api/v1/security/retention/holds', False, False, False),
         'security_retention_hold_create': ('post', '/api/v1/security/retention/holds', False, True, True),
         'security_retention_hold_delete': ('delete', '/api/v1/security/retention/holds/{id}', False, True, False),
         'security_retention_proof_list': ('get', '/api/v1/security/retention/proofs', False, False, False),
         'security_residency_check': ('post', '/api/v1/security/residency/check', False, False, False),
         'security_protected_group_list': ('get', '/api/v1/security/protected-groups', False, False, False),
         'security_protected_group_sync_class': ('put',
                                                 '/api/v1/security/protected-groups/{group}/sync-class',
                                                 False,
                                                 True,
                                                 False),
         'security_node_key_list': ('get', '/api/v1/security/node-keys', False, False, False),
         'security_node_key_revoke': ('post', '/api/v1/security/node-keys/{node}/revoke', False, True, False),
         'security_support_request_list': ('get', '/api/v1/security/support-access/requests', False, False, False),
         'security_support_request_consent': ('post',
                                              '/api/v1/security/support-access/requests/{id}/consent',
                                              False,
                                              True,
                                              False),
         'security_support_request_revoke': ('post',
                                             '/api/v1/security/support-access/requests/{id}/revoke',
                                             False,
                                             True,
                                             False),
         'security_support_session_list': ('get', '/api/v1/security/support-access/sessions', False, False, False),
         'security_support_session_recording': ('get',
                                                '/api/v1/security/support-access/sessions/{id}/recording',
                                                False,
                                                True,
                                                False),
         'security_audit_chain_verify': ('post', '/api/v1/security/audit/chain/verify', False, False, False),
         'security_audit_entries': ('get', '/api/v1/security/audit/entries', False, False, False),
         'security_audit_export': ('get', '/api/v1/security/audit/export', False, True, False),
         'security_compliance_evidence_pack': ('post',
                                               '/api/v1/security/compliance/evidence-pack',
                                               False,
                                               False,
                                               True),
         'security_compliance_controls': ('get', '/api/v1/security/compliance/controls', False, False, False)},
 'once': [],
 'codes': {'security.not_found': (404, False),
           'security.invalid': (422, False),
           'security.plan_required': (422, False),
           'security.plan_changed': (409, False),
           'security.consent_invalid': (422, False),
           'security.request_not_pending': (409, False)},
 'schemas': ['SecurityAuditChainVerifyRequest',
             'SecurityAuditChainVerifyResponse',
             'SecurityAuditEntriesResponse',
             'SecurityAuditExportResponse',
             'SecurityComplianceControlsResponse',
             'SecurityComplianceEvidencePackRequest',
             'SecurityComplianceEvidencePackResponse',
             'SecurityDataPolicy',
             'SecurityDataPolicyGetResponse',
             'SecurityDataPolicyPutRequest',
             'SecurityDataPolicyPutResponse',
             'SecurityNodeKey',
             'SecurityNodeKeyListResponse',
             'SecurityNodeKeyRevokeResponse',
             'SecurityProtectedGroup',
             'SecurityProtectedGroupListResponse',
             'SecurityProtectedGroupSyncClassRequest',
             'SecurityProtectedGroupSyncClassResponse',
             'SecurityResidencyCheckRequest',
             'SecurityResidencyCheckResponse',
             'SecurityRetentionHold',
             'SecurityRetentionHoldCreateRequest',
             'SecurityRetentionHoldCreateResponse',
             'SecurityRetentionHoldListResponse',
             'SecurityRetentionProofListResponse',
             'SecurityRetentionRun',
             'SecurityRetentionRunCreateRequest',
             'SecurityRetentionRunCreateResponse',
             'SecurityRetentionRunListResponse',
             'SecuritySupportRequest',
             'SecuritySupportRequestConsentRequest',
             'SecuritySupportRequestConsentResponse',
             'SecuritySupportRequestListResponse',
             'SecuritySupportRequestRevokeResponse',
             'SecuritySupportSession',
             'SecuritySupportSessionListResponse',
             'SecuritySupportSessionRecordingResponse'],
 'writes_need_step_up': [('/api/v1/security/',
                          ['security_residency_check',
                           'security_audit_chain_verify',
                           'security_compliance_evidence_pack'])],
 'body_requires_when': {'security_retention_run_create': 'plan_digest'}}


# Rule 27 (contract 0.21.0, WP-CON-03.T50): the data import operations (upload, dry run, run with progress, errors, cancel).
GROUP_PINS[27] = {'version': '0.21.0',
 'task': 'WP-CON-03.T50',
 'ops': {'import_limits': ('get', '/api/v1/import/limits', False, False, False),
         'import_create': ('post', '/api/v1/import', False, False, False),
         'import_get': ('get', '/api/v1/import/{id}', False, False, False),
         'import_errors': ('get', '/api/v1/import/{id}/errors', False, False, False),
         'import_run': ('post', '/api/v1/import/{id}/run', False, False, False),
         'import_cancel': ('post', '/api/v1/import/{id}/cancel', False, False, False)},
 'once': [],
 'codes': {'import.file_unreadable': (422, False),
           'import.mapping_invalid': (422, False),
           'import.limit_exceeded': (413, False),
           'import.not_ready': (409, False),
           'import.digest_changed': (409, False),
           'import.cancelled': (409, False),
           'import.right_missing': (403, False)},
 'schemas': ['ImportCancelResponse',
             'ImportCreateRequest',
             'ImportCreateResponse',
             'ImportErrorsResponse',
             'ImportGetResponse',
             'ImportJob',
             'ImportLimitsResponse',
             'ImportMapping',
             'ImportRunRequest',
             'ImportRunResponse']}


# Rule 28 (contract 0.22.0, WP-CON-03.T51): full-text search on the resource query operation (search, rank order, facets, snippets) and the searchable and facetable fields in the metadata.
GROUP_PINS[28] = {'version': '0.22.0',
 'task': 'WP-CON-03.T51',
 'ops': {},
 'once': [],
 'codes': {'search.unsupported': (501, False),
           'search.term_too_long': (422, False),
           'search.field_not_searchable': (422, False),
           'search.projection_behind': (503, True)},
 'schemas': [],
 'schema_props': {'QueryRequest': ['search', 'facets', 'snippets'],
                  'DocumentPage': ['facets', 'snippets', 'projection_lag'],
                  'DocTypeMeta': ['searchable', 'facetable'],
                  'OrderKey': ['rank']},
 'schema_values': [['QueryRequest', ['properties', 'search', 'properties', 'text', 'maxLength'], 512],
                   ['QueryRequest', ['properties', 'search', 'properties', 'fields', 'maxItems'], 16],
                   ['QueryRequest', ['properties', 'facets', 'maxItems'], 4],
                   ['QueryRequest', ['properties', 'snippets', 'properties', 'max_chars', 'maximum'], 200]],
 'op_words': {'resource_query': ['search', 'rank', 'one extra unit', 'permission.field_denied']}}


# Rule 29 (contract 0.23.0, WP-CON-03.T52): booking holds (claim, confirm, release) and availability over the occupancy entity.
GROUP_PINS[29] = {'version': '0.23.0',
 'task': 'WP-CON-03.T52',
 'ops': {'occupancy_claim': ('post', '/api/v1/occupancy/{entity}/holds', False, False, True),
         'occupancy_confirm': ('post', '/api/v1/occupancy/{entity}/holds/{id}/confirm', False, False, True),
         'occupancy_release': ('delete', '/api/v1/occupancy/{entity}/holds/{id}', False, False, False),
         'occupancy_availability': ('get', '/api/v1/occupancy/{entity}/availability', False, False, False)},
 'once': [],
 'codes': {'occupancy.conflict': (409, False),
           'occupancy.hold_expired': (409, False),
           'occupancy.invalid_range': (422, False),
           'occupancy.ttl_too_long': (422, False),
           'occupancy.not_holder': (403, False)},
 'schemas': ['OccupancyAvailabilityResponse',
             'OccupancyClaimRequest',
             'OccupancyClaimResponse',
             'OccupancyConfirmResponse',
             'OccupancyHold']}


# Rule 30 (contract 0.24.0, WP-CON-03.T53): the MCP endpoint contract (contracts/native-api/mcp.md) and its pointer in openapi.yaml.
GROUP_PINS[30] = {'version': '0.24.0',
 'task': 'WP-CON-03.T53',
 'ops': {},
 'once': [],
 'codes': {},
 'schemas': [],
 'doc_words': {'mcp.md': ['2026-07-28',
                          'POST /mcp',
                          'Origin',
                          'WWW-Authenticate',
                          'Bearer',
                          'X-Api-Key',
                          'filtered per principal',
                          'fails a write-capable call closed',
                          'job_status',
                          '256 KiB',
                          'not CORS-open']}}


# Rule 31 (contract 0.25.0, WP-CON-03.T54): the installed apps list and the connector operations (list, get, invoke, run log, inbound).
GROUP_PINS[31] = {'version': '0.25.0',
 'task': 'WP-CON-03.T54',
 'ops': {'app_list': ('get', '/api/v1/apps', False, False, False),
         'connector_list': ('get', '/api/v1/connectors', False, False, False),
         'connector_get': ('get', '/api/v1/connectors/{name}', False, False, False),
         'connector_invoke': ('post', '/api/v1/connectors/{name}/operations/{op}', False, False, True),
         'connector_runs': ('get', '/api/v1/connectors/{name}/runs', False, False, False),
         'connector_inbound': ('post', '/api/v1/connectors/{name}/inbound', True, False, False)},
 'once': [],
 'codes': {'connector.not_found': (404, False),
           'connector.disabled': (409, False),
           'connector.operation_unknown': (404, False),
           'connector.input_invalid': (422, False),
           'connector.upstream_failed': (502, True),
           'connector.deadline_exceeded': (504, False),
           'connector.signature_invalid': (401, False),
           'connector.replayed': (409, False)},
 'schemas': ['AppListResponse',
             'Connector',
             'ConnectorGetResponse',
             'ConnectorInboundRequest',
             'ConnectorInboundResponse',
             'ConnectorInvokeRequest',
             'ConnectorInvokeResponse',
             'ConnectorListResponse',
             'ConnectorRunsResponse',
             'InstalledApp'],
 'op_words': {'connector_inbound': ['constant time', '1 MiB', '5 minute window', 'no oracle', 'service principal'],
              'connector_list': ['no credential']}}


# Rule 33 (contract 0.26.1, WP-CON-03.T67): the external peer's inbound operation (public, HMAC-signed, one uniform 401, 429 reuses api.rate_limited).
GROUP_PINS[33] = {'version': '0.26.1',
 'task': 'WP-CON-03.T67',
 'ops': {'link_inbound': ('post', '/api/v1/link/inbound/{peer}', True, False, False)},
 'once': [],
 'codes': {'link_inbound.unauthorized': (401, False)},
 'schemas': ['LinkInboundError', 'LinkInboundRequest', 'LinkInboundResponse'],
 'op_words': {'link_inbound': ['5 minute window', 'never a `413`', 'api.rate_limited', 'no request id', 'Retry-After']}}


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
    check_processes_and_tasks(spec, errors)
    check_process_versions(spec, errors)
    check_graph_and_lineage(spec, errors)
    check_signing_secret(spec, errors)
    check_assignment_create_answers(spec, errors)
    check_journal_and_selectors(spec, errors)
    for rule_no, pin in GROUP_PINS.items():
        check_group(rule_no, pin, spec, errors)
        check_group_extra(rule_no, pin, spec, errors)
    if realtime_text is not None:
        check_notification(realtime_text, errors)
    return errors, len(codes), n


def check_assignment_create_answers(spec: dict, errors: list[str]) -> None:
    """Rule 32 (0.25.1): identity_assignment_create answers 201 (assignment only) or 202 (grant_request_id only)."""
    op = spec["paths"]["/api/v1/identity/assignments"]["post"]
    want = {"201": ("IdentityAssignmentCreateResponse", "assignment"),
            "202": ("IdentityAssignmentGrantRequestedResponse", "grant_request_id")}
    for status, (name, prop) in want.items():
        resp = (op.get("responses") or {}).get(status)
        ref = (((resp or {}).get("content") or {}).get("application/json") or {}).get("schema", {}).get("$ref")
        if ref != f"#/components/schemas/{name}":
            errors.append(f"rule 32: identity_assignment_create must answer {status} with {name}")
            continue
        data = spec["components"]["schemas"].get(name, {}).get("properties", {}).get("data", {})
        if data.get("required") != [prop] or list(data.get("properties", {})) != [prop]:
            errors.append(f"rule 32: {name} must carry exactly `{prop}`")


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
    pt = "/api/v1/process"
    rule18 = {
        "a missing operation": lambda s: s["paths"].pop("/api/v1/task/{id}/reassign"),
        "a bearer-less task read": lambda s: s["paths"]["/api/v1/task"]["get"].update(security=[]),
        "a bearer-ful webhook": lambda s: s["paths"][pt + "/hook/{trigger}"]["post"].pop("security"),
        "a webhook that answers 200": lambda s: s["paths"][pt + "/hook/{trigger}"]["post"]["responses"].update(
            {"200": {"description": "x"}}),
        "an unaudited write": lambda s: s["paths"]["/api/v1/task/{id}/claim"]["post"].update(
            description=s["paths"]["/api/v1/task/{id}/claim"]["post"]["description"].replace("audited", "logged")),
        "a run start without an idempotency key": lambda s: s["paths"][pt + "/{process}/runs"]["post"].pop("parameters"),
        "a completion without If-Match": lambda s: s["paths"]["/api/v1/task/{id}/complete"]["post"][
            "parameters"].pop(0),
        "a missing no-oracle 404": lambda s: s["paths"][pt + "/runs/{id}"]["get"].update(description="The run."),
        "an unbounded list": lambda s: s["paths"]["/api/v1/task"]["get"]["parameters"][3]["schema"].update(maximum=500),
        "a user query parameter": lambda s: s["paths"]["/api/v1/task"]["get"]["parameters"].append(
            {"name": "user", "in": "query", "schema": {"type": "string"}}),
        "an unregistered code": lambda s: s["x-spot-error-codes"].pop("task.segregation"),
        "a wrong status": lambda s: s["x-spot-error-codes"].update({"task.not_found": [403, False, "x"]}),
        "an open request body": lambda s: schemas(s)["TaskComplete"].pop("additionalProperties"),
        "an unbounded string": lambda s: schemas(s)["RunCancel"]["properties"]["reason"].pop("maxLength"),
        "an unbounded array": lambda s: schemas(s)["Run"]["properties"]["steps"].pop("maxItems"),
        "an unbounded values map": lambda s: schemas(s)["TaskComplete"]["properties"]["values"].pop("maxProperties"),
        "run variables in a run": lambda s: schemas(s)["Run"]["properties"].update(variables={"type": "object", "maxProperties": 1}),
        "an actor in the completion body": lambda s: schemas(s)["TaskComplete"]["properties"].update(
            actor={"type": "string", "maxLength": 10}),
    }
    for label, mutate in rule18.items():
        errors, _, _ = run_rules(broken(mutate))
        if not any(e.startswith("rule 18") for e in errors):
            print(f"self-test: {label} (rule 18) was not detected; got {errors[:3]}")
            failures += 1
    vp = "/api/v1/process/{process}/versions"
    rule19 = {
        "a missing operation": lambda s: s["paths"].pop(vp + "/{n}/retire"),
        "a bearer-less version read": lambda s: s["paths"][vp]["get"].update(security=[]),
        "an unaudited publish": lambda s: s["paths"][vp]["post"].update(
            description=s["paths"][vp]["post"]["description"].replace("audited", "logged")),
        "a publish without an idempotency key": lambda s: s["paths"][vp]["post"].pop("parameters"),
        "a migration start without an idempotency key": lambda s: s["paths"][vp + "/{from}/migrations"]["post"].pop(
            "parameters"),
        "a missing permission statement": lambda s: s["paths"][vp + "/{from}/migrations/plan"]["post"].update(
            description="A dry run. 409 process.version_not_published 422 process.migration_invalid audited"),
        "a missing no-oracle 404": lambda s: s["paths"][vp + "/{n}"]["get"].update(
            description="The version. `process.read_versions` content hash"),
        "a get without the content-hash ETag": lambda s: s["paths"][vp + "/{n}"]["get"]["responses"]["200"].pop("headers"),
        "an unbounded version list": lambda s: s["paths"][vp]["get"]["parameters"][1]["schema"].update(maximum=500),
        "a user query parameter": lambda s: s["paths"][vp]["get"]["parameters"].append(
            {"name": "user", "in": "query", "schema": {"type": "string"}}),
        "an unregistered code": lambda s: s["x-spot-error-codes"].pop("process.version_conflict"),
        "a wrong status": lambda s: s["x-spot-error-codes"].update({"process.migration_not_found": [403, False, "x"]}),
        "an open publish body": lambda s: schemas(s)["ProcessVersionPublish"].pop("additionalProperties"),
        "an unbounded definition": lambda s: schemas(s)["ProcessVersionPublish"]["properties"]["definition"].pop("maxLength"),
        "a definition over 256 KiB": lambda s: schemas(s)["ProcessVersionPublish"]["properties"]["definition"].update(
            maxLength=300000),
        "an unbounded note": lambda s: schemas(s)["ProcessVersionPublish"]["properties"]["note"].update(maxLength=5000),
        "an unbounded array": lambda s: schemas(s)["MigrationPlan"]["properties"]["unmoved"].pop("maxItems"),
        "an unbounded node map": lambda s: schemas(s)["MigrationMapping"]["properties"]["nodes"].pop("maxProperties"),
        "an actor in the publish body": lambda s: schemas(s)["ProcessVersionPublish"]["properties"].update(
            actor={"type": "string", "maxLength": 10}),
        "a value in an unmoved run": lambda s: schemas(s)["MigrationUnmoved"]["properties"].update(
            variables={"type": "string", "maxLength": 10}),
        "an unnamed service principal rule": lambda s: s["paths"][vp]["post"].update(
            description=s["paths"][vp]["post"]["description"].replace("process.publish_denied", "x")),
        "an unnamed insight report": lambda s: s["paths"]["/api/v1/report"]["get"].update(
            description=s["paths"]["/api/v1/report"]["get"]["description"].replace("bpm_stuck_queue", "stuck")),
    }
    for label, mutate in rule19.items():
        errors, _, _ = run_rules(broken(mutate))
        if not any(e.startswith("rule 19") for e in errors):
            print(f"self-test: {label} (rule 19) was not detected; got {errors[:3]}")
            failures += 1
    gq, lq = "/api/v1/graph/query", "/api/v1/lineage"
    rule20 = {
        "a missing graph operation": lambda s: s["paths"].pop(gq),
        "a missing lineage operation": lambda s: s["paths"].pop(lq),
        "a bearer-less graph read": lambda s: s["paths"][gq]["post"].update(security=[]),
        "an idempotency key on a read": lambda s: s["paths"][lq]["post"].update(
            parameters=[{"$ref": "#/components/parameters/IdempotencyKeyRequired"}]),
        "a graph answer with no no-oracle 404": lambda s: s["paths"][gq]["post"].update(
            description=s["paths"][gq]["post"]["description"].replace("`404 graph.not_found`", "404")),
        "a missing withheld statement": lambda s: s["paths"][gq]["post"].update(
            description=s["paths"][gq]["post"]["description"].replace("removed with every path through it", "dropped")),
        "an unstated truncation": lambda s: s["paths"][lq]["post"].update(
            description=s["paths"][lq]["post"]["description"].replace("never a timeout", "a timeout").replace("and never a timeout", "or a timeout")),
        "a lineage quantity not stated as decimal": lambda s: s["paths"][lq]["post"].update(
            description=s["paths"][lq]["post"]["description"].replace("exact decimal strings", "numbers")),
        "an unregistered graph code": lambda s: s["x-spot-error-codes"].pop("graph.not_found"),
        "a wrong status": lambda s: s["x-spot-error-codes"].update({"graph.unavailable": [503, False, "x"]}),
        "an unregistered lineage code": lambda s: s["x-spot-error-codes"].pop("lineage.kind_unknown"),
        "an open graph query": lambda s: schemas(s)["GraphQuery"].pop("additionalProperties"),
        "a depth of 9": lambda s: schemas(s)["GraphQuery"]["properties"]["depth"].update(maximum=9),
        "a limit of 5000": lambda s: schemas(s)["GraphQuery"]["properties"]["limit"].update(maximum=5000),
        "too many edge types": lambda s: schemas(s)["GraphQuery"]["properties"]["edge_types"].update(maxItems=100),
        "an unbounded edge type list": lambda s: schemas(s)["GraphQuery"]["properties"]["edge_types"].pop("maxItems"),
        "a lineage depth of 20": lambda s: schemas(s)["LineageRequest"]["properties"]["depth"].update(maximum=20),
        "a lineage limit of 50000": lambda s: schemas(s)["LineageRequest"]["properties"]["limit"].update(maximum=50000),
        "an invented filter operator": lambda s: schemas(s)["GraphFilterCompare"]["properties"]["op"].update(
            enum=["eq", "ne", "lt", "le", "gt", "ge", "like"]),
        "an invented filter form": lambda s: schemas(s)["GraphFilter"]["oneOf"].append(
            {"$ref": "#/components/schemas/GraphFilterNull"}),
        "a node with a field value": lambda s: schemas(s)["GraphNodeRef"]["properties"].update(
            name={"type": "string", "maxLength": 10}),
        "a withheld count": lambda s: schemas(s)["GraphAnswer"]["properties"].update(withheld={"type": "integer"}),
        "a withheld that is optional": lambda s: schemas(s)["LineageAnswer"].update(required=["levels", "truncated"]),
        "a numeric quantity": lambda s: schemas(s)["LineageNode"]["properties"].update(quantity={"type": "number"}),
        "a client chain configuration": lambda s: schemas(s)["LineageRequest"]["properties"].update(
            chain_config={"type": "object", "maxProperties": 5}),
        "a site in the graph query": lambda s: schemas(s)["GraphQuery"]["properties"].update(
            site={"type": "string", "maxLength": 10}),
        "an unbounded string in an answer": lambda s: schemas(s)["GraphEdgeProperties"]["additionalProperties"]["oneOf"][0].pop(
            "maxLength"),
        "an unbounded level array": lambda s: schemas(s)["LineageAnswer"]["properties"]["levels"].pop("maxItems"),
        "a truncation reason of time": lambda s: schemas(s)["GraphTruncationReason"].update(enum=["depth", "time"]),
        "an unknown lineage kind": lambda s: schemas(s)["LineageKind"].update(enum=["chain", "bom"]),
    }
    for label, mutate in rule20.items():
        errors, _, _ = run_rules(broken(mutate))
        if not any(e.startswith("rule 20") for e in errors):
            print(f"self-test: {label} (rule 20) was not detected; got {errors[:3]}")
            failures += 1
    sr = "/api/v1/webhooks/{name}/signing-secret"
    rule21 = {
        "a missing put": lambda s: s["paths"][sr].pop("put"),
        "a missing get": lambda s: s["paths"][sr].pop("get"),
        "a bearer-less get": lambda s: s["paths"][sr]["get"].update(security=[]),
        "a secret in a query string": lambda s: s["paths"][sr]["get"].update(
            parameters=[{"name": "secret", "in": "query", "schema": {"type": "string", "maxLength": 9}}]),
        "an unnamed administrator rule": lambda s: s["paths"][sr]["put"].update(
            description=s["paths"][sr]["put"]["description"].replace("Tenant Administrator", "administrator")),
        "an existence oracle": lambda s: s["paths"][sr]["get"].update(
            description=s["paths"][sr]["get"]["description"].replace("whether or not the webhook exists", "whether it exists")),
        "a cacheable put": lambda s: s["paths"][sr]["put"]["responses"]["200"].pop("headers"),
        "an unaudited put": lambda s: s["paths"][sr]["put"].update(
            description=s["paths"][sr]["put"]["description"].replace("audited", "logged")),
        "a put with an idempotency key": lambda s: s["paths"][sr]["put"].update(
            parameters=[{"$ref": "#/components/parameters/IdempotencyKeyRequired"}]),
        "a readable value": lambda s: s["paths"][sr]["get"].update(
            description=s["paths"][sr]["get"]["description"].replace("The value is never returned", "The value")),
        "a body with both": lambda s: schemas(s)["WebhookSigningSecretPut"].pop("oneOf"),
        "an open put body": lambda s: schemas(s)["WebhookSigningSecretPut"].pop("additionalProperties"),
        "an unbounded secret": lambda s: schemas(s)["WebhookSigningSecretPut"]["properties"]["secret"].update(maxLength=100000),
        "a secret in the get answer": lambda s: schemas(s)["WebhookSigningSecretStateResponse"]["properties"]["data"]["properties"].update(
            secret={"type": "string", "maxLength": 9}),
        "a value in the put answer": lambda s: schemas(s)["WebhookSigningSecretPutResponse"]["properties"]["data"]["properties"].update(
            value={"type": "string", "maxLength": 9}),
        "a long fingerprint": lambda s: schemas(s)["WebhookSigningSecretPutResponse"]["properties"]["data"]["properties"]["fingerprint"].update(maxLength=64),
        "an unbounded generated secret": lambda s: schemas(s)["WebhookSigningSecretPutResponse"]["properties"]["data"]["properties"]["secret"].pop("maxLength"),
        "a registered catalogue code": lambda s: s["x-spot-error-codes"].update({"webhook_secret.refused": [403, False, "x"]}),
        "an unregistered refusal": lambda s: s["x-spot-error-codes"].pop("permission.denied"),
    }
    for label, mutate in rule21.items():
        errors, _, _ = run_rules(broken(mutate))
        if not any(e.startswith("rule 21") for e in errors):
            print(f"self-test: {label} (rule 21) was not detected; got {errors[:3]}")
            failures += 1
    jr = "/api/v1/journal/segments/{generation}/{first_seq}-{last_seq}"
    rule22 = {
        "a missing info": lambda s: s["paths"].pop("/api/v1/journal"),
        "a missing manifest": lambda s: s["paths"].pop("/api/v1/journal/manifest/{generation}"),
        "a missing segment": lambda s: s["paths"].pop(jr),
        "a bearer-less segment": lambda s: s["paths"][jr]["get"].update(security=[]),
        "an unnamed role": lambda s: s["paths"]["/api/v1/journal"]["get"].update(
            description=s["paths"]["/api/v1/journal"]["get"]["description"].replace("journal_reader", "reader")),
        "no grant_expired rule": lambda s: s["paths"]["/api/v1/journal/manifest/{generation}"]["get"].update(
            description=s["paths"]["/api/v1/journal/manifest/{generation}"]["get"]["description"].replace("grant_expired", "gone")),
        "a filtered-looking segment": lambda s: s["paths"][jr]["get"].update(
            description=s["paths"][jr]["get"]["description"].replace("WHOLE TENANT", "tenant")),
        "no range": lambda s: s["paths"][jr]["get"]["responses"].pop("206"),
        "no etag": lambda s: s["paths"][jr]["get"]["responses"]["200"]["headers"].pop("ETag"),
        "a cacheable manifest": lambda s: s["paths"]["/api/v1/journal/manifest/{generation}"]["get"]["responses"]["200"].pop("headers"),
        "a query parameter": lambda s: s["paths"]["/api/v1/journal"].update(
            parameters=[{"name": "token", "in": "query", "schema": {"type": "string", "maxLength": 9}}]),
        "telemetry as a class": lambda s: schemas(s)["ConsumerSpec"]["properties"]["classes"]["items"]["enum"].append("telemetry"),
        "unbounded classes": lambda s: schemas(s)["ConsumerSpec"]["properties"]["classes"].pop("maxItems"),
        "unbounded entities": lambda s: schemas(s)["ConsumerSpec"]["properties"]["entities"].pop("maxItems"),
        "required classes": lambda s: schemas(s)["ConsumerSpec"]["required"].append("classes"),
        "a wrong served_from": lambda s: schemas(s)["PullBatch"]["properties"]["served_from"].update(enum=["outbox"]),
        "a required served_from": lambda s: schemas(s)["PullBatch"]["required"].append("served_from"),
        "an undocumented history rule": lambda s: s["paths"]["/api/v1/events/stream"]["get"].update(
            description=s["paths"]["/api/v1/events/stream"]["get"]["description"].replace("journal", "archive")),
        "a missing journal code": lambda s: s["x-spot-error-codes"].pop("journal.halted"),
        "a non-retryable unknown outcome": lambda s: s["x-spot-error-codes"].update(
            {"journal.commit_outcome_unknown": [503, False, "x"]}),
    }
    for label, mutate in rule22.items():
        errors, _, _ = run_rules(broken(mutate))
        if not any(e.startswith("rule 22") for e in errors):
            print(f"self-test: {label} (rule 22) was not detected; got {errors[:3]}")
            failures += 1
    group_total = 0
    for rule, pin in GROUP_PINS.items():
        if not pin["ops"]:
            continue  # an addition-only version (0.22.0 search, 0.24.0 MCP) has its own explicit rule
        op_id, (method, route, public, step_up, idem) = next(iter(pin["ops"].items()))
        first_code = next(iter(pin["codes"])) if pin["codes"] else "permission.denied"
        first_schema = pin["schemas"][0]
        getter = next((o for o, v in pin["ops"].items() if v[0] == "get" and o not in pin.get("once", [])), None)
        gmethod, groute = (pin["ops"][getter][0], pin["ops"][getter][1]) if getter else (None, None)
        group_muts = {
            "a missing operation": lambda s, route=route, method=method: s["paths"][route].pop(method),
            "an unstated permission": lambda s, route=route, method=method: s["paths"][route][method].update(
                description=s["paths"][route][method]["description"].replace("Permission:", "Perm:")),
            "an unstated audience": lambda s, route=route, method=method: s["paths"][route][method].update(
                description=s["paths"][route][method]["description"].replace("Audience:", "Who:")),
            "a flipped security": lambda s, route=route, method=method, public=public: (
                s["paths"][route][method].pop("security", None) if public else s["paths"][route][method].update(security=[])),
            "an unregistered code": lambda s, c=first_code: s["x-spot-error-codes"].pop(c),
            "an open schema": lambda s, n=first_schema: schemas(s)[n].pop("additionalProperties", None) if "properties" in schemas(s)[n] else schemas(s)[n].update(type="object", properties={"x": {"type": "string"}}),
            "a secret in a query string": lambda s, route=route, method=method: s["paths"][route][method].setdefault(
                "parameters", []).append({"name": "api_key", "in": "query", "schema": {"type": "string", "maxLength": 9}}),
        }
        if step_up:
            group_muts["an unstated step-up"] = lambda s, route=route, method=method: s["paths"][route][method].update(
                description=s["paths"][route][method]["description"].replace("step-up", "login"))
        if getter:
            def leak(s, groute=groute, gmethod=gmethod):
                ref = s["paths"][groute][gmethod]["responses"]["200"]["content"]["application/json"]["schema"]["$ref"]
                s["components"]["schemas"][ref.rsplit("/", 1)[-1]]["properties"]["secret"] = {"type": "string", "maxLength": 9}
            group_muts["a secret in a read answer"] = leak
        for label, mutate in group_muts.items():
            errors = []
            check_group(rule, pin, broken(mutate), errors)
            if not errors:
                print(f"self-test: {label} (rule {rule}) was not detected; got {errors[:3]}")
                failures += 1
            group_total += 1
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
             + len(key_breaks) + len(rule18) + len(rule19) + len(rule20) + len(rule21) + len(rule22) + group_total + 2)
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
