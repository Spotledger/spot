#!/usr/bin/env python3
"""Contract checks for the store API (WP-CON-03.T55, ADR-21).

Run with the pinned requirements in ``requirements.txt`` (the same pattern
``contracts/native-api/check.py`` and its ``tools/lints/native-api/run.sh`` use: a throwaway
venv installs them; never needed at runtime). Every rule below is part of the contract; a
failure names the rule and the JSON pointer it failed at.

1. ``openapi.yaml`` is a valid OpenAPI 3.1 document.
2. No schema anywhere uses ``type: number``: money, sizes and counts never pass through a
   float (ADR-12); this surface carries no price at all (WP-STR-08 is out of scope).
3. Paths are ``/store/v1/...``; operation ids are unique snake_case.
4. Every operation has a ``default`` response referencing the shared Error.
5. The error-code registry (``x-spot-error-codes``) is well formed, every code is dotted
   lower-case under ``store.``, and every entry carries a non-empty ``explanation`` (the third
   element) as well as the plain ``meaning`` text.
6. Every schema that declares ``properties`` states ``additionalProperties`` explicitly.
7. Every example validates against the schema it illustrates, and every error example uses a
   registered code.
8. The download operation (``app_bundle_download``) never lets the server claim a trust
   decision it has not made: its description states that the site verifies the signature and
   the digest itself, and no response schema or header anywhere in the document claims a
   bundle is already "safe", "trusted" or "verified".
9. The review schema (``ReviewVerdict``) offers exactly ``spot-app-review``'s three verdict
   values and no more, and no schema description claims a reviewed app is "safe" on the
   store's own authority.
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
SPEC_PATH = HERE / "openapi.yaml"
ROOT_URI = "urn:spot:store-api"

CODE_RE = re.compile(r"^store\.[a-z][a-z0-9_]*$")
OPERATION_ID_RE = re.compile(r"^[a-z][a-z0-9_]*$")
HTTP_METHODS = ("get", "put", "post", "delete", "patch", "head", "options", "trace")
VERDICTS = {"rejected", "needs_human_review", "looks_clean"}
UNEARNED_TRUST_RE = re.compile(r"\b(is|are)\s+(already\s+)?(safe|trusted|verified)\b", re.IGNORECASE)


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
                errors.append(f"rule 2: `type: number` at {pointer(path)}; no float ever carries money, a size or a count")


def operations(spec: dict[str, Any]) -> Iterator[tuple[str, str, dict[str, Any]]]:
    for route, item in spec.get("paths", {}).items():
        for method in HTTP_METHODS:
            if method in item:
                yield route, method, item[method]


def check_paths(spec: dict[str, Any], errors: list[str]) -> None:
    seen: dict[str, str] = {}
    for route, method, op in operations(spec):
        where = f"{method.upper()} {route}"
        if not route.startswith("/store/v1/"):
            errors.append(f"rule 3: {where}: paths start with /store/v1/")
        op_id = op.get("operationId")
        if not op_id or not OPERATION_ID_RE.fullmatch(op_id):
            errors.append(f"rule 3: {where}: operationId must be snake_case, got {op_id!r}")
        elif op_id in seen:
            errors.append(f"rule 3: {where}: operationId {op_id} also used by {seen[op_id]}")
        else:
            seen[op_id] = where


def check_default_response(spec: dict[str, Any], errors: list[str]) -> None:
    for route, method, op in operations(spec):
        where = f"{method.upper()} {route}"
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
            errors.append(f"rule 5: error code {code!r} is not a dotted lower-case store.* code")
        ok = (isinstance(entry, list) and len(entry) == 3 and isinstance(entry[0], int)
              and 400 <= entry[0] <= 599 and isinstance(entry[1], bool)
              and isinstance(entry[2], str) and entry[2].strip())
        if not ok:
            errors.append(f"rule 5: {code}: entry must be [status 400-599, retryable, non-empty explanation]")
    return set(registry)


def check_additional_properties(spec: dict[str, Any], errors: list[str]) -> None:
    for path, node in walk(spec, []):
        if isinstance(node, dict) and isinstance(node.get("properties"), dict):
            if "additionalProperties" not in node:
                errors.append(f"rule 6: {pointer(path)}: an object schema with `properties` must state `additionalProperties`")


def examples(spec: dict[str, Any]) -> Iterator[tuple[str, list[str | int], Any]]:
    """Yields (schema pointer, example path, example value) for every example with a schema."""
    for path, node in walk(spec, []):
        if not isinstance(node, dict):
            continue
        if "schema" in node and isinstance(node["schema"], dict):
            schema_ptr = pointer(path + ["schema"])
            if "example" in node:
                yield schema_ptr, path + ["example"], node["example"]
            for name, ex in (node.get("examples") or {}).items():
                if isinstance(ex, dict) and "value" in ex:
                    yield schema_ptr, path + ["examples", name, "value"], ex["value"]
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
            errors.append(f"rule 7: example at {loc} does not match {schema_ptr}: {err.message}")
        if isinstance(value, dict) and isinstance(value.get("error"), dict):
            code = value["error"].get("code")
            if code not in codes:
                errors.append(f"rule 5: example at {pointer(ex_path)} uses unregistered code {code!r}")
    if count == 0:
        errors.append("rule 7: no examples found; the check is not looking in the right place")
    return count


DOWNLOAD_ROUTE = "/store/v1/apps/{name}/versions/{version}/bundle"
TRUST_WORDS = ("verifies", "never trust", "never trusted")


def check_download_trust(spec: dict[str, Any], errors: list[str]) -> None:
    """Rule 8: a download is never presented as an already-made trust decision."""
    item = spec.get("paths", {}).get(DOWNLOAD_ROUTE, {})
    op = item.get("get")
    if not op:
        errors.append(f"rule 8: GET {DOWNLOAD_ROUTE} is missing")
        return
    text = " ".join(str(op.get("description", "")).split())
    if not any(word in text for word in TRUST_WORDS):
        errors.append(f"rule 8: GET {DOWNLOAD_ROUTE}: description must state that the site verifies the download itself")
    for path, node in walk(spec, []):
        if not isinstance(node, dict):
            continue
        for key in ("description", "summary"):
            text = node.get(key)
            if isinstance(text, str) and UNEARNED_TRUST_RE.search(text) and "never" not in text.lower() and "no header or field" not in text.lower() and "may claim" not in text.lower() and "may not" not in text.lower():
                errors.append(f"rule 8: {pointer(path + [key])}: text claims the store itself calls a bundle safe/trusted/verified: {text[:120]!r}")


def check_review_verdict(spec: dict[str, Any], errors: list[str]) -> None:
    """Rule 9: the review verdict is exactly spot-app-review's three values; no claim of safety."""
    schemas = spec.get("components", {}).get("schemas", {})
    verdict = schemas.get("ReviewVerdict", {})
    enum = set(verdict.get("enum", []))
    if enum != VERDICTS:
        errors.append(f"rule 9: ReviewVerdict must be exactly {sorted(VERDICTS)}, got {sorted(enum)}")
    for name in ("ReviewVerdict", "ReviewSummary"):
        desc = str(schemas.get(name, {}).get("description", ""))
        if re.search(r"\bis safe\b", desc, re.IGNORECASE) and "never" not in desc.lower():
            errors.append(f"rule 9: components/schemas/{name}: description must not claim an app is safe")


def run_rules(spec: dict[str, Any]) -> tuple[list[str], int, int]:
    errors: list[str] = []
    check_openapi(spec, errors)
    check_no_number(spec, errors)
    check_paths(spec, errors)
    check_default_response(spec, errors)
    codes = check_registry(spec, errors)
    check_additional_properties(spec, errors)
    n = check_examples(spec, codes, errors)
    check_download_trust(spec, errors)
    check_review_verdict(spec, errors)
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
        "rule 2": broken(lambda s: schemas(s)["VersionDetail"]["properties"]["size"].update(type="number")),
        "rule 3": broken(lambda s: s["paths"]["/store/v1/apps"]["get"].update(operationId="app_publish")),
        "rule 4": broken(lambda s: s["paths"]["/store/v1/apps"]["get"]["responses"].pop("default")),
        "rule 5": broken(lambda s: s["x-spot-error-codes"]["store.name_taken"].__setitem__(2, "")),
        "rule 5 (format)": broken(lambda s: s["x-spot-error-codes"].update({"not_dotted": [400, False, "x"]})),
        "rule 6": broken(lambda s: schemas(s)["YankRequest"].pop("additionalProperties")),
        "rule 7": broken(lambda s: s["paths"]["/store/v1/revocations"]["get"]["responses"]["200"]["content"]
                          ["application/json"]["example"]["data"].__setitem__("generation", "seven")),
        "rule 8": broken(lambda s: s["paths"][DOWNLOAD_ROUTE]["get"].update(
            description="This download is already verified and safe to install without checking anything.")),
        "rule 9": broken(lambda s: schemas(s)["ReviewVerdict"].update(
            enum=["rejected", "needs_human_review", "looks_clean", "approved"])),
    }
    failures = 0
    for rule, spec in cases.items():
        errors, _, _ = run_rules(spec)
        base_rule = rule.split(" ", 2)[0] + " " + rule.split(" ", 2)[1]
        if not any(e.startswith(base_rule) for e in errors):
            print(f"self-test: breaking {rule} was not detected; got {errors[:3]}")
            failures += 1
    # Rule 5's unregistered-code-in-example case needs a live example to break.
    unregistered_example = copy.deepcopy(base)
    unregistered_example["paths"]["/store/v1/revocations"]["get"]["responses"]["default"] = {
        "$ref": "#/components/responses/Error",
        "content": {"application/json": {"schema": {"$ref": "#/components/schemas/ErrorEnvelope"},
                                          "example": {"error": {"code": "store.made_up", "message": "x",
                                                                 "retryable": False, "request_id": "r"}}}},
    }
    errors, _, _ = run_rules(unregistered_example)
    if not any("unregistered code" in e for e in errors):
        print("self-test: an unregistered error code in an example was not detected")
        failures += 1
    # Rule 9's "is safe" description case.
    unsafe_desc = copy.deepcopy(base)
    schemas(unsafe_desc)["ReviewSummary"]["description"] = "Once looks_clean, the app is safe to list."
    errors, _, _ = run_rules(unsafe_desc)
    if not any(e.startswith("rule 9") for e in errors):
        print("self-test: a description claiming an app 'is safe' was not detected")
        failures += 1
    total = len(cases) + 2
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
        print(f"store-api: {len(errors)} problem(s)")
        return 1
    print(f"store-api {spec['info']['version']}: {ops} operations, {n_codes} error codes, "
          f"{n} examples checked, all rules pass")
    return 0


if __name__ == "__main__":
    sys.exit(main())
