#!/usr/bin/env python3
"""Emit one CycloneDX 1.5 software bill of materials for the repository
(WP-PRD-08.T2).

Sources:

* Rust: ``cargo metadata --format-version 1 --locked`` on the workspace
  manifest. Workspace crates become ``application`` components; every other
  package becomes a ``library`` with its SPDX licence expression and, when the
  lock file carries one, the SHA-256 checksum. purl: ``pkg:cargo/<name>@<version>``.
* .NET: every ``packages.lock.json`` checked in under the repository root.
  There are none today, so the emitter skips the section with a message until
  lock files land (enable ``RestorePackagesWithLockFile`` and commit them).
  The lock file records a sha512 content hash, written as a CycloneDX SHA-512
  hash; project-to-project entries are internal code, not packages, and are
  dropped. purl: ``pkg:nuget/<name>@<version>``.

The output is deterministic: components are sorted, JSON keys are sorted, and
nothing time-dependent is written unless ``--timestamp`` is passed.

Exit codes: 0 written, 2 the SBOM could not be produced (tool failure or an
invalid document).
"""
from __future__ import annotations

import argparse
import base64
import datetime
import json
import re
import subprocess
import sys
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[2]
SPEC_VERSION = "1.5"
TOOL_NAME = "tools/security/sbom.py"
TOOL_VERSION = "1.0.0"
TOOL_TIMEOUT = 300
LOCK_FILE = "packages.lock.json"
_SKIPPED_TREES = ("target", ".git")


class SbomError(RuntimeError):
    """A data source could not be read; the SBOM would be incomplete or wrong."""


class CargoMissing(SbomError):
    """The cargo executable is not on PATH; the Rust section is skipped."""


def run_cargo_metadata(root: Path) -> dict:
    """Workspace metadata as JSON. --locked keeps Cargo.lock untouched."""
    argv = ["cargo", "metadata", "--format-version", "1", "--locked",
            "--manifest-path", str((Path(root) / "Cargo.toml").resolve())]
    try:
        proc = subprocess.run(argv, cwd=str(root), capture_output=True, text=True,
                              timeout=TOOL_TIMEOUT)
    except FileNotFoundError as exc:
        raise CargoMissing("cargo is not on PATH") from exc
    except subprocess.TimeoutExpired as exc:
        raise SbomError(f"cargo metadata timed out after {TOOL_TIMEOUT}s") from exc
    if proc.returncode != 0:
        tail = "\n".join((proc.stderr or proc.stdout or "").splitlines()[-5:])
        raise SbomError(f"cargo metadata exited {proc.returncode}:\n{tail}")
    return json.loads(proc.stdout)


def rust_components(metadata: dict) -> list[dict]:
    """CycloneDX components from ``cargo metadata`` output."""
    members = set(metadata.get("workspace_members") or [])
    components: list[dict] = []
    for package in metadata.get("packages") or []:
        name, version = str(package.get("name", "")).strip(), str(package.get("version", "")).strip()
        if not name or not version:
            raise SbomError(f"package without name/version: {package.get('id')!r}")
        purl = f"pkg:cargo/{name}@{version}"
        component: dict = {
            "type": "application" if package.get("id") in members else "library",
            "name": name,
            "version": version,
            "purl": purl,
            "bom-ref": purl,
        }
        licence, licence_file = package.get("license"), package.get("license_file")
        if licence:
            component["licenses"] = [{"license": {"expression": str(licence)}}]
        elif licence_file:
            component["licenses"] = [{"license": {"name": str(licence_file)}}]
        if package.get("source") and package.get("checksum"):
            component["hashes"] = [{"alg": "SHA-256", "content": str(package["checksum"])}]
        components.append(component)
    return components


def find_lock_files(root: Path) -> list[Path]:
    """Every checked-in packages.lock.json, outside build and checkout dirs."""
    return sorted(path for path in Path(root).rglob(LOCK_FILE)
                  if not (set(path.parts) & set(_SKIPPED_TREES)))

_SHA512_CONTENT = re.compile(r"^sha512-(?P<b64>[A-Za-z0-9+/=]+)$")


def parse_content_hash(value: str) -> dict | None:
    """NuGet ``contentHash`` (``sha512-<base64>``) as a CycloneDX SHA-512 hash."""
    match = _SHA512_CONTENT.match((value or "").strip())
    if not match:
        return None
    try:
        digest = base64.b64decode(match.group("b64"), validate=True)
    except ValueError:
        return None
    return {"alg": "SHA-512", "content": digest.hex()}


def nuget_components(lock_files: list[Path]) -> list[dict]:
    """CycloneDX components from packages.lock.json files (project refs dropped).

    The same package can appear per target framework; components are merged by
    purl, whichever lock file names the content hash first (locks are visited
    sorted, so the result is deterministic).
    """
    merged: dict[str, dict] = {}
    for lock in sorted(Path(path) for path in lock_files):
        try:
            data = json.loads(lock.read_text(encoding="utf-8"))
        except (OSError, json.JSONDecodeError) as exc:
            raise SbomError(f"{lock}: unreadable lock file: {exc}") from exc
        dependencies = data.get("dependencies") or {}
        for tfm in sorted(dependencies):
            table = dependencies[tfm] or {}
            for name in sorted(table):
                entry = table[name] or {}
                if str(entry.get("type", "")).lower() == "project":
                    continue
                resolved = str(entry.get("resolved", "")).strip()
                if not resolved:
                    continue
                purl = f"pkg:nuget/{name}@{resolved}"
                component = merged.get(purl)
                if component is None:
                    component = {"type": "library", "name": name, "version": resolved,
                                 "purl": purl, "bom-ref": purl}
                    merged[purl] = component
                hash_info = parse_content_hash(str(entry.get("contentHash", "") or ""))
                if hash_info and "hashes" not in component:
                    component["hashes"] = [hash_info]
    return list(merged.values())


def build_sbom(rust: list[dict], nuget: list[dict],
               timestamp: str | None = None) -> dict:
    """Assemble the CycloneDX 1.5 document; sorted and timestamp-free by default."""
    components = sorted((*rust, *nuget), key=lambda c: (c["purl"], c["name"], c["version"]))
    document: dict = {
        "bomFormat": "CycloneDX",
        "specVersion": SPEC_VERSION,
        "version": 1,
        "metadata": {"tools": {"components": [
            {"type": "application", "name": TOOL_NAME, "version": TOOL_VERSION},
        ]}},
        "components": components,
    }
    if timestamp:
        document["metadata"]["timestamp"] = timestamp
    return document


def validate_sbom(document: dict) -> list[str]:
    """Check the CycloneDX 1.5 fields this emitter relies on (no schema download)."""
    errors: list[str] = []
    if document.get("bomFormat") != "CycloneDX":
        errors.append("bomFormat must be 'CycloneDX'")
    if document.get("specVersion") != SPEC_VERSION:
        errors.append(f"specVersion must be '{SPEC_VERSION}'")
    if not isinstance(document.get("version"), int):
        errors.append("version must be an integer")
    components = document.get("components")
    if not isinstance(components, list):
        errors.append("components must be a list")
        components = []
    for index, component in enumerate(components):
        where = f"components[{index}]"
        for key in ("type", "name", "version", "purl", "bom-ref"):
            if not str(component.get(key, "")).strip():
                errors.append(f"{where}: missing '{key}'")
        hashes = component.get("hashes")
        if hashes is not None:
            for position, entry in enumerate(hashes):
                if not str(entry.get("alg", "")).strip() or not str(entry.get("content", "")).strip():
                    errors.append(f"{where}: hashes[{position}] needs 'alg' and 'content'")
        licences = component.get("licenses")
        if licences is not None:
            for position, entry in enumerate(licences):
                inner = entry.get("license") if isinstance(entry, dict) else None
                if not isinstance(inner, dict) or not (str(inner.get("expression", "")).strip()
                                                       or str(inner.get("name", "")).strip()):
                    errors.append(f"{where}: licenses[{position}] needs license.expression "
                                  "or license.name")
    return errors


def timestamp_now() -> str:
    return datetime.datetime.now(datetime.timezone.utc).isoformat(
        timespec="seconds").replace("+00:00", "Z")


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        description="Emit a deterministic CycloneDX 1.5 SBOM for the repository.")
    parser.add_argument("--out", type=Path, required=True, help="where to write the SBOM JSON")
    parser.add_argument("--timestamp", action="store_true",
                        help="add a timestamp (breaks the byte-for-byte determinism)")
    parser.add_argument("--root", type=Path, default=REPO_ROOT,
                        help="repository root (default: the checkout containing this script)")
    args = parser.parse_args(argv)
    try:
        rust: list[dict] = []
        if (args.root / "Cargo.toml").is_file():
            try:
                rust = rust_components(run_cargo_metadata(args.root))
            except CargoMissing as exc:
                print(f"SKIP: {exc}; the Rust section is empty")
        else:
            print(f"SKIP: no Cargo.toml under {args.root}; the Rust section is empty")
        locks = find_lock_files(args.root)
        if locks:
            nuget = nuget_components(locks)
        else:
            nuget = []
            print("SKIP: no packages.lock.json found under the repository root; the "
                  ".NET section is empty (enable RestorePackagesWithLockFile and commit "
                  "the lock files to fill it in)")
        document = build_sbom(rust, nuget, timestamp=timestamp_now() if args.timestamp else None)
        errors = validate_sbom(document)
        if errors:
            for error in errors:
                print(f"ERROR: {error}", file=sys.stderr)
            return 2
        args.out.parent.mkdir(parents=True, exist_ok=True)
        args.out.write_text(json.dumps(document, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    except (SbomError, OSError, ValueError) as exc:
        print(f"ERROR: {exc}", file=sys.stderr)
        return 2
    applications = sum(1 for component in document["components"]
                       if component["type"] == "application")
    print(f"wrote {args.out}: {len(document['components'])} components "
          f"({applications} application, {len(document['components']) - applications} library)")
    return 0


if __name__ == "__main__":
    sys.exit(main())