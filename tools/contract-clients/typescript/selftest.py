"""The exactness self-test: the output is compared with the contract read a
second time, by code that shares nothing with the generator's model.
"""

from __future__ import annotations

import hashlib
import json
import re
import shutil
import subprocess
import tempfile
from pathlib import Path
from typing import Any

import yaml

from tsemit import TABLE_BEGIN, TABLE_END

METHODS = ("get", "put", "post", "delete", "patch", "head", "options", "trace")


def _walk_refs(node: Any, found: list[str]) -> None:
    if isinstance(node, dict):
        for key, value in node.items():
            if key == "$ref" and isinstance(value, str):
                found.append(value)
            else:
                _walk_refs(value, found)
    elif isinstance(node, list):
        for item in node:
            _walk_refs(item, found)


def _pointer(doc: Any, ref: str) -> Any:
    node = doc
    for part in ref[2:].split("/"):
        token = part.replace("~1", "/").replace("~0", "~")
        node = node[int(token)] if isinstance(node, list) else node[token]
    return node


def contract_counts(contract: Path) -> dict[str, int]:
    doc = yaml.safe_load(contract.read_text(encoding="utf-8"))
    ops = sum(1 for item in doc.get("paths", {}).values() for m in METHODS if m in item)
    return {"operations": ops, "schemas": len(doc.get("components", {}).get("schemas", {}))}


def _contract_ops(doc: Any) -> dict[str, dict[str, Any]]:
    out: dict[str, dict[str, Any]] = {}
    for path, item in doc.get("paths", {}).items():
        for method in METHODS:
            if method not in item:
                continue
            op = item[method]
            merged: dict[tuple[str, str], bool] = {}
            for raw in list(item.get("parameters") or []) + list(op.get("parameters") or []):
                param = _pointer(doc, raw["$ref"]) if "$ref" in raw else raw
                merged[(param["name"], param["in"])] = bool(param.get("required")) or param["in"] == "path"
            out[op["operationId"]] = {"method": method.upper(), "path": path, "params": merged}
    return out


def read_table(operations_ts: str) -> dict[str, Any]:
    start = operations_ts.index(TABLE_BEGIN) + len(TABLE_BEGIN)
    end = operations_ts.index(TABLE_END)
    return json.loads(operations_ts[start:end])


def run(contract: Path, version: str, out_dir: Path, counts: dict[str, int]) -> list[str]:
    """Return the findings (empty when the output is exact)."""
    findings: list[str] = []
    raw = contract.read_bytes()
    doc = yaml.safe_load(raw.decode("utf-8"))

    # every $ref resolves
    refs: list[str] = []
    _walk_refs(doc, refs)
    for ref in sorted(set(refs)):
        try:
            _pointer(doc, ref)
        except (KeyError, IndexError, ValueError):
            findings.append(f"$ref does not resolve: {ref}")

    expected = contract_counts(contract)
    for key in ("operations", "schemas"):
        if counts.get(key) != expected[key]:
            findings.append(f"count {key}: report says {counts.get(key)}, contract has {expected[key]}")

    # operations table
    ops = _contract_ops(doc)
    table = read_table((out_dir / "operations.ts").read_text(encoding="utf-8"))
    for oid in sorted(set(ops) - set(table)):
        findings.append(f"operation {oid} has no table entry")
    for oid in sorted(set(table) - set(ops)):
        findings.append(f"table entry {oid} is not in the contract")
    for oid in sorted(set(ops) & set(table)):
        want, got = ops[oid], table[oid]
        if got["path"] != want["path"]:
            findings.append(f"{oid}: table path {got['path']!r} != contract {want['path']!r}")
        if got["method"] != want["method"]:
            findings.append(f"{oid}: table method {got['method']!r} != contract {want['method']!r}")
        for loc, key in (("path", "pathParams"), ("query", "queryParams"), ("header", "headerParams")):
            wanted = {(n, r) for (n, w), r in want["params"].items() if w == loc}
            have = {(p["name"], p["required"]) for p in got[key]}
            if wanted != have:
                findings.append(f"{oid}: {key} {sorted(have)} != contract {sorted(wanted)}")

    # client methods: exactly one per operation
    client = (out_dir / "client.ts").read_text(encoding="utf-8")
    methods = re.findall(r'^  ([A-Za-z_][A-Za-z0-9_]*)\(.*Promise<Operations\["', client, flags=re.M)
    for oid in sorted(ops):
        n = methods.count(oid)
        if n != 1:
            findings.append(f"operation {oid} has {n} client methods (want exactly 1)")
    for extra in sorted(set(methods) - set(ops)):
        findings.append(f"client method {extra} is not an operation of the contract")

    # types: exactly one export per schema
    types = (out_dir / "types.ts").read_text(encoding="utf-8")
    exported = re.findall(r"^export (?:interface|type) ([A-Za-z_][A-Za-z0-9_]*)\b", types, flags=re.M)
    schemas = set(doc.get("components", {}).get("schemas", {}))
    for name in sorted(schemas):
        if exported.count(name) != 1:
            findings.append(f"schema {name} has {exported.count(name)} exported types (want exactly 1)")
    for extra in sorted(set(exported) - schemas):
        findings.append(f"exported type {extra} is not a schema of the contract")

    # the contract version rides along
    contract_ts = (out_dir / "contract.ts").read_text(encoding="utf-8")
    if f'CONTRACT_VERSION = {json.dumps(version)};' not in contract_ts:
        findings.append("contract.ts does not carry the contract version")
    if f'CONTRACT_SHA256 = "{hashlib.sha256(raw).hexdigest()}";' not in contract_ts:
        findings.append("contract.ts does not carry the contract sha256")
    return findings


def type_check(out_dir: Path, tsc: str | None, require: bool) -> list[str]:
    """Compile the output with ``tsc --strict`` when a compiler is available."""
    if tsc is None:
        if require:
            return ["tsc not available and SPOT_REQUIRE_TSC=1"]
        print("tsc not available: type check skipped")
        return []
    cmd = tsc.split() + [
        "--strict", "--noEmit", "--target", "ES2022", "--lib", "ES2022",
        "--module", "ES2022", "--moduleResolution", "bundler", "--skipLibCheck", "false",
    ] + sorted(str(p) for p in out_dir.glob("*.ts"))
    done = subprocess.run(cmd, capture_output=True, text=True, check=False)
    if done.returncode != 0:
        return ["tsc failed:\n" + (done.stdout + done.stderr).strip()]
    return []


def find_tsc(env: dict[str, str]) -> str | None:
    named = env.get("SPOT_TSC")
    if named:
        return named
    found = shutil.which("tsc")
    return found


def scratch() -> Path:
    return Path(tempfile.mkdtemp(prefix="spot-ts-surface-"))
