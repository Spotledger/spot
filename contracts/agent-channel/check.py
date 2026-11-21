#!/usr/bin/env python3
"""Contract checks for the Spotledger host-agent channel (WP-CON-03.T62).

Run with the pinned requirements in ``requirements.txt`` (the pattern of ``contracts/native-api/check.py``:
a throwaway venv installs them; never needed at runtime). Every rule is part of the contract; a failure
names the rule and what it failed at. ``--self-test`` breaks the spec once per rule and fails unless every
break is detected.

1. ``openapi.yaml`` is a valid OpenAPI 3.1 document.
2. No schema uses ``type: number``: money, sizes and counts never pass through a float (ADR-12).
3. Paths start with ``/agent/v1``; operation ids are unique snake_case.
4. Every operation has a ``default`` response referencing the shared Error.
5. The error-code registry (``x-spot-error-codes``) is well formed: dotted lower case, a status, a
   retryable flag and a meaning; the codes of this contract carry its prefix; every "<status> <code>" an
   operation description names is registered with exactly that status.
6. Every pinned operation exists on its method and path, is public only when the pin says, and states its
   permission, audience, rate class and (for a write) its audit; no query parameter is named like a secret.
7. No response property is named like a secret or a key (a shown-once secret is not part of this contract).
8. Every pinned schema is closed and bounded (strings, arrays and open maps carry a limit).
9. The pinned words appear in the operations and in the contract text that the rules of the contract rest on.
10. ``CHANGELOG.md`` has a section for the current ``VERSION``.
"""

from __future__ import annotations

import copy
import re
import sys
from pathlib import Path
from typing import Any

import yaml
from openapi_spec_validator import validate as validate_openapi

HERE = Path(__file__).resolve().parent
SPEC_PATH = HERE / "openapi.yaml"
PREFIX = "/agent/v1"
CODE_PREFIXES = ('agent.',)
SECRET_NAMES = {"secret", "client_secret", "password", "token", "private_key", "api_key", "key_material", "key", "recovery_codes"}

PIN: dict[str, Any] = {'version': '0.1.0',
 'task': 'WP-CON-03.T62',
 'ops': {'agent_enrol': ('post', '/agent/v1/enrol', True, False, False),
         'agent_poll': ('post', '/agent/v1/poll', False, False, False),
         'agent_results': ('post', '/agent/v1/results', False, False, True),
         'agent_heartbeat': ('post', '/agent/v1/heartbeat', False, False, False),
         'agent_update_get': ('get', '/agent/v1/update', False, False, False)},
 'once': [],
 'sec': {},
 'codes': {'agent.token_invalid': (401, False),
           'agent.unauthenticated': (401, False),
           'agent.replayed': (409, False),
           'agent.revoked': (403, False),
           'agent.unsupported': (422, False),
           'agent.invalid': (422, False),
           'agent.not_found': (404, False),
           'agent.body_too_large': (413, False),
           'agent.rate_limited': (429, True),
           'agent.update_unavailable': (404, False),
           'agent.internal': (500, False)},
 'schemas': ['AgentCommand',
             'AgentEnrolRequest',
             'AgentEnrolResponse',
             'AgentHeartbeatRequest',
             'AgentPollRequest',
             'AgentPollResponse',
             'AgentResultsRequest',
             'AgentSignedCommand',
             'AgentUpdateGetResponse',
             'AgentUpdateManifest'],
 'info_words': ['outbound',
                'method, path, body digest, timestamp and a request nonce',
                'cannot forge a command',
                'Before it executes',
                'at most once',
                '64 KiB',
                'redact',
                'agent.revoked',
                'Key rotation',
                '2 minutes',
                'monotonic',
                'downgrade',
                'signed rollback',
                'agent.unsupported'],
 'op_words': {'agent_enrol': ['agent.token_invalid'], 'agent_results': ['64 KiB']},
 'values': [[['info', 'version'], '0.1.0']]}


def load() -> dict[str, Any]:
    with SPEC_PATH.open(encoding="utf-8") as f:
        return yaml.safe_load(f)


def operations(spec: dict[str, Any]):
    for path, item in spec.get("paths", {}).items():
        for method, op in item.items():
            if method in ("get", "put", "post", "patch", "delete"):
                yield path, method, op


def walk(node: Any, where: str = ""):
    yield where, node
    if isinstance(node, dict):
        for k, v in node.items():
            yield from walk(v, f"{where}/{k}")
    elif isinstance(node, list):
        for i, v in enumerate(node):
            yield from walk(v, f"{where}/{i}")


def props(spec: dict[str, Any], node: Any, seen: set[str]):
    if not isinstance(node, dict):
        return
    if "$ref" in node:
        name = node["$ref"].rsplit("/", 1)[-1]
        if name not in seen:
            seen.add(name)
            yield from props(spec, spec.get("components", {}).get("schemas", {}).get(name), seen)
        return
    for prop, sub in node.get("properties", {}).items():
        yield prop
        yield from props(spec, sub, seen)
    for key in ("items", "additionalProperties"):
        yield from props(spec, node.get(key), seen)
    for key in ("oneOf", "allOf", "anyOf"):
        for sub in node.get(key, []):
            yield from props(spec, sub, seen)


def closed(node: Any, where: str, errors: list[str]) -> None:
    if not isinstance(node, dict) or "$ref" in node:
        return
    types = node.get("type")
    types = [types] if isinstance(types, str) else (types or [])
    if "object" in types:
        if "properties" in node:
            if node.get("additionalProperties") is not False:
                errors.append(f"rule 8: {where}: an object schema here is closed (additionalProperties: false)")
            for name, sub in node["properties"].items():
                closed(sub, f"{where}.{name}", errors)
        elif "maxProperties" not in node:
            errors.append(f"rule 8: {where}: an open object needs maxProperties")
        else:
            closed(node.get("additionalProperties"), f"{where}.*", errors)
    if "string" in types and not any(k in node for k in ("maxLength", "enum", "const")):
        errors.append(f"rule 8: {where}: a string needs maxLength (or an enum)")
    if "array" in types:
        if "maxItems" not in node:
            errors.append(f"rule 8: {where}: an array needs maxItems")
        closed(node.get("items"), f"{where}[]", errors)
    for key in ("oneOf", "allOf"):
        for i, sub in enumerate(node.get(key, [])):
            closed(sub, f"{where}.{key}[{i}]", errors)


def run_rules(spec: dict[str, Any]) -> list[str]:
    errors: list[str] = []
    try:
        validate_openapi(spec)
    except Exception as exc:  # noqa: BLE001
        errors.append(f"rule 1: not a valid OpenAPI document: {str(exc)[:300]}")
        return errors
    for where, node in walk(spec):
        if isinstance(node, dict) and node.get("type") == "number":
            errors.append(f"rule 2: {where}: type number is not allowed")
    registry = spec.get("x-spot-error-codes", {})
    seen_ids: set[str] = set()
    for path, method, op in operations(spec):
        if not path.startswith(PREFIX):
            errors.append(f"rule 3: {path} does not start with {PREFIX}")
        op_id = op.get("operationId", "")
        if not re.fullmatch(r"[a-z][a-z0-9_]*", op_id) or op_id in seen_ids:
            errors.append(f"rule 3: operation id {op_id!r} is not unique snake_case")
        seen_ids.add(op_id)
        if op.get("responses", {}).get("default") != {"$ref": "#/components/responses/Error"}:
            errors.append(f"rule 4: {op_id} has no default response referencing Error")
        text = " ".join(str(op.get("description", "")).split())
        for status, code in re.findall(r"\b([1-5][0-9][0-9]) ([a-z][a-z0-9_]*\.[a-z0-9_.]+)", text):
            row = registry.get(code)
            if row is None or row[0] != int(status):
                errors.append(f"rule 5: {op_id} names {status} {code}, which is not registered with that status")
    for code, row in registry.items():
        if not re.fullmatch(r"[a-z][a-z0-9_]*(\.[a-z][a-z0-9_]*)+", code):
            errors.append(f"rule 5: error code {code!r} is not dotted lower case")
        if not (isinstance(row, list) and len(row) == 3 and isinstance(row[0], int) and isinstance(row[1], bool) and isinstance(row[2], str) and row[2]):
            errors.append(f"rule 5: error code {code} must be [status, retryable, meaning]")
    for code in PIN["codes"]:
        if code not in registry or registry[code][0] != PIN["codes"][code][0] or registry[code][1] is not PIN["codes"][code][1]:
            errors.append(f"rule 5: error code {code} must be registered as {list(PIN['codes'][code])}")
        if not code.startswith(CODE_PREFIXES):
            errors.append(f"rule 5: error code {code} does not carry this contract's prefix")
    paths = spec.get("paths", {})
    for op_id, (method, route, public, *_rest) in PIN["ops"].items():
        op = paths.get(route, {}).get(method)
        if not op or op.get("operationId") != op_id:
            errors.append(f"rule 6: {method.upper()} {route} ({op_id}) is missing")
            continue
        if public and op.get("security") != []:
            errors.append(f"rule 6: {op_id} is public and says so (`security: []`)")
        want = PIN.get("sec", {}).get(op_id)
        if not public and want is None and "security" in op:
            errors.append(f"rule 6: {op_id} uses the contract's security (no override)")
        if want is not None and op.get("security") != [{want: []}]:
            errors.append(f"rule 6: {op_id} is secured by {want}")
        text = " ".join(str(op.get("description", "")).split())
        words = ["Permission:", "Audience:", "Rate class"] + (["Audited:"] if method != "get" else [])
        for word in words + PIN.get("op_words", {}).get(op_id, []):
            if word not in text:
                errors.append(f"rule 6: {op_id} does not state {word!r}")
        for p in op.get("parameters", []) + paths.get(route, {}).get("parameters", []):
            if p.get("in") == "query" and re.search(r"token|secret|password|key|signature", str(p.get("name", "")), re.I):
                errors.append(f"rule 6: {op_id}: query parameter {p.get('name')!r} is named like a secret")
        for status, resp in op.get("responses", {}).items():
            if status == "default":
                continue
            for content in resp.get("content", {}).values():
                for prop in props(spec, content.get("schema"), set()):
                    if prop in SECRET_NAMES:
                        errors.append(f"rule 7: {op_id}: response property {prop!r} is named like a secret or a key")
    for name in PIN["schemas"]:
        schema = spec.get("components", {}).get("schemas", {}).get(name)
        if schema is None:
            errors.append(f"rule 8: schema {name} is missing")
        else:
            closed(schema, name, errors)
    text_all = " ".join(str(spec.get("info", {}).get("description", "")).split())
    for word in PIN.get("info_words", []):
        if word not in text_all:
            errors.append(f"rule 9: the contract text does not state {word!r}")
    for field, want in PIN.get("values", []):
        node: Any = spec
        for part in field:
            node = node.get(part, {}) if isinstance(node, dict) else {}
        if node != want:
            errors.append(f"rule 9: {'/'.join(map(str, field))} must be {want!r}")
    log = (HERE / "CHANGELOG.md").read_text(encoding="utf-8") if (HERE / "CHANGELOG.md").exists() else ""
    version = (HERE / "VERSION").read_text(encoding="utf-8").strip() if (HERE / "VERSION").exists() else ""
    if f"## {version}" not in log:
        errors.append(f"rule 10: CHANGELOG.md has no section for {version}")
    if spec.get("info", {}).get("version") != version:
        errors.append("rule 10: info.version and VERSION differ")
    return errors


def self_test() -> int:
    base = load()
    failures = 0
    total = 0

    def broken(mutate):
        spec = copy.deepcopy(base)
        mutate(spec)
        return spec

    first_id, (m, r, pub, *_x) = next(iter(PIN["ops"].items()))
    secured = next((i for i, v in PIN["ops"].items() if not v[2]), first_id)
    sm, sr = PIN["ops"][secured][0], PIN["ops"][secured][1]
    first_schema = PIN["schemas"][0]
    first_code = next(iter(PIN["codes"]))
    cases = {
        "a missing operation": lambda s: s["paths"][r].pop(m),
        "an unstated permission": lambda s: s["paths"][sr][sm].update(description=s["paths"][sr][sm]["description"].replace("Permission:", "Perm:")),
        "an unstated audience": lambda s: s["paths"][sr][sm].update(description=s["paths"][sr][sm]["description"].replace("Audience:", "Who:")),
        "a security override": lambda s: s["paths"][sr][sm].update(security=[{"nope": []}]),
        "an unregistered code": lambda s: s["x-spot-error-codes"].pop(first_code),
        "a wrong status": lambda s: s["x-spot-error-codes"].update({first_code: [299, False, "x"]}),
        "a float": lambda s: s["components"]["schemas"][first_schema].update(type="number"),
        "an open schema": lambda s: s["components"]["schemas"][first_schema].pop("additionalProperties", None) if "properties" in s["components"]["schemas"][first_schema] else s["components"]["schemas"][first_schema].update(type="object", properties={"x": {"type": "string"}}),
        "a missing default": lambda s: s["paths"][r][m]["responses"].pop("default"),
        "a secret query parameter": lambda s: s["paths"][sr][sm].setdefault("parameters", []).append({"name": "api_key", "in": "query", "schema": {"type": "string", "maxLength": 9}}),
        "a foreign path": lambda s: s["paths"].update({"/other/x": {"get": {"operationId": "other_x", "responses": {"default": {"$ref": "#/components/responses/Error"}}}}}),
    }
    if PIN.get("info_words"):
        word = PIN["info_words"][0]
        cases["an unstated rule"] = lambda s, w=word: s["info"].update(description=s["info"]["description"].replace(w, "x"))
    for label, mutate in cases.items():
        total += 1
        if not run_rules(broken(mutate)):
            print(f"self-test: {label} was not detected")
            failures += 1
    print(f"self-test: {total - failures} of {total} broken specs detected")
    return 1 if failures else 0


def main() -> int:
    if "--self-test" in sys.argv[1:]:
        return self_test()
    spec = load()
    errors = run_rules(spec)
    for e in errors:
        print(e)
    ops = sum(1 for _ in operations(spec))
    if errors:
        print(f"agent-channel: {len(errors)} problem(s)")
        return 1
    print(f"agent-channel {spec['info']['version']}: {ops} operations, {len(spec.get('x-spot-error-codes', {}))} error codes, all rules pass")
    return 0


if __name__ == "__main__":
    sys.exit(main())
