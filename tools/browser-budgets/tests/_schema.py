"""A minimal OpenAPI 3.1 schema reader for the stub contract tests (WP-UI-09.T5).

`contracts/native-api/check.py` validates with `jsonschema`, which is a pinned
tool of its own virtualenv and not installed where these tests run (the brief
forbids a new Python package). This reader covers exactly the keywords the
native API schemas use for the shapes the stub returns: $ref, type (including
type lists), const, enum, pattern, minimum, minLength, required, properties,
additionalProperties (bool or schema), items and allOf. A keyword it does not
know raises, so a contract change cannot be silently skipped.
"""

from __future__ import annotations

import re
from pathlib import Path

try:  # PyYAML is what check.py loads the contract with.
    import yaml
except ImportError:  # pragma: no cover - the tests skip with a message
    yaml = None

SPEC = Path(__file__).resolve().parents[3] / "contracts" / "native-api" / "openapi.yaml"

KNOWN = {
    "$ref", "type", "const", "enum", "pattern", "minimum", "maximum", "minLength", "maxLength",
    "required", "properties", "additionalProperties", "items", "allOf", "maxItems", "minItems",
    # annotations
    "description", "example", "examples", "default", "title", "format", "deprecated",
    "x-spot-secret", "x-spot-since",
    # openapi.yaml writes `description: Total digits, for decimals.` unquoted in two places, which YAML
    # reads as the extra keys below (value null). They are noise, not constraints; report to the lead.
    "for decimals.", "documented per code.",
}


def load_spec():
    if yaml is None:
        raise RuntimeError("PyYAML is not installed")
    return yaml.safe_load(SPEC.read_text(encoding="utf-8"))


class Validator:
    def __init__(self, spec):
        self.spec = spec

    def resolve(self, ref):
        if not ref.startswith("#/"):
            raise ValueError(f"unsupported $ref {ref}")
        node = self.spec
        for part in ref[2:].split("/"):
            node = node[part.replace("~1", "/").replace("~0", "~")]
        return node

    def errors(self, schema, value, where="$"):
        if "$ref" in schema:
            return self.errors(self.resolve(schema["$ref"]), value, where)
        unknown = {key for key in schema if key not in KNOWN and not key.startswith("x-")}
        if unknown:
            raise ValueError(f"{where}: the test reader does not know {sorted(unknown)}")
        found = []
        if "allOf" in schema:
            for part in schema["allOf"]:
                found += self.errors(part, value, where)
        if "type" in schema:
            types = schema["type"] if isinstance(schema["type"], list) else [schema["type"]]
            if not any(self._is(kind, value) for kind in types):
                return found + [f"{where}: {value!r} is not {types}"]
        if "const" in schema and value != schema["const"]:
            found.append(f"{where}: not {schema['const']!r}")
        if "enum" in schema and value not in schema["enum"]:
            found.append(f"{where}: {value!r} not in {schema['enum']}")
        if isinstance(value, str):
            if "pattern" in schema and not re.search(schema["pattern"], value):
                found.append(f"{where}: {value!r} does not match {schema['pattern']}")
            if "minLength" in schema and len(value) < schema["minLength"]:
                found.append(f"{where}: too short")
            if "maxLength" in schema and len(value) > schema["maxLength"]:
                found.append(f"{where}: too long")
        if isinstance(value, int) and not isinstance(value, bool):
            if "minimum" in schema and value < schema["minimum"]:
                found.append(f"{where}: below the minimum")
            if "maximum" in schema and value > schema["maximum"]:
                found.append(f"{where}: above the maximum")
        if isinstance(value, list):
            if "items" in schema:
                for index, item in enumerate(value):
                    found += self.errors(schema["items"], item, f"{where}[{index}]")
            if "minItems" in schema and len(value) < schema["minItems"]:
                found.append(f"{where}: too few items")
            if "maxItems" in schema and len(value) > schema["maxItems"]:
                found.append(f"{where}: too many items")
        if isinstance(value, dict):
            for name in schema.get("required", []):
                if name not in value:
                    found.append(f"{where}: missing {name}")
            props = schema.get("properties", {})
            extra = schema.get("additionalProperties", True)
            for name, item in value.items():
                if name in props:
                    found += self.errors(props[name], item, f"{where}.{name}")
                elif extra is False:
                    found.append(f"{where}: unexpected {name}")
                elif isinstance(extra, dict):
                    found += self.errors(extra, item, f"{where}.{name}")
        return found

    @staticmethod
    def _is(kind, value):
        return {
            "object": isinstance(value, dict),
            "array": isinstance(value, list),
            "string": isinstance(value, str),
            "integer": isinstance(value, int) and not isinstance(value, bool),
            "boolean": isinstance(value, bool),
            "null": value is None,
            "number": isinstance(value, float),
        }[kind]
