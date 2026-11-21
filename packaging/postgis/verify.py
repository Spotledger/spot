#!/usr/bin/env python3
"""Check the desktop PostgreSQL + PostGIS bundle manifest, and optionally a staged bundle.

`MANIFEST.toml` is the pinned bill of materials for the PostgreSQL and PostGIS binaries the
Windows desktop installer ships (WP-PRD-02.T7, ADR-087 sections 4.2, 4.3 and 11.3 S2). This
script is the checked part of that file:

  * it validates the manifest's *shape* — the keys the installer and `spot-pg-supervisor`
    rely on, the roles, relative paths, one entry per file, every required platform
    present — so a malformed edit fails a cheap check instead of the installer;
  * with ``--files <dir>`` it verifies the *content*: every declared file exists under the
    unpacked prefix with the declared byte size and SHA-256.

Standard library only (Python 3.11+ for ``tomllib``); no network access, ever: the hashes
are pinned in the manifest, and a hash is only as good as the download it came from (each
one records its evidence in the manifest).

Exit codes
----------
0   the manifest is valid (and, with ``--files``, every pinned hash matched)
1   the manifest is invalid, or a declared file is missing, the wrong size, or hashed
    differently
2   the manifest is otherwise fine but still carries a ``todo = "pin before release"``
    value, so it is not shippable yet (no release may be cut from it)

Usage
-----
    python3 packaging/postgis/verify.py --manifest packaging/postgis/MANIFEST.toml
    python3 packaging/postgis/verify.py --manifest packaging/postgis/MANIFEST.toml \\
        --files C:/build/pgsql --platform windows-x86_64
"""

from __future__ import annotations

import argparse
import hashlib
import re
import sys
from pathlib import Path
from typing import Any, NamedTuple

try:  # Python 3.11+
    import tomllib
except ModuleNotFoundError:  # pragma: no cover - the check below reports it
    tomllib = None  # type: ignore[assignment]

#: The literal marker a value carries when it could not be verified before release. A
#: fabricated hash is worse than an absent one, so the manifest says so out loud.
TODO = "pin before release"

#: What a declared file is for. `control` and `sql` are the extension's own files, `library`
#: its shared modules (and the ones they load), `program` the server binaries, `licence` a
#: notice the installer must carry, `data` anything else the bundle needs at run time.
ROLES = frozenset({"program", "control", "sql", "library", "licence", "data"})

#: Where the manifest's own schema version lives. Bump it when the shape changes.
SCHEMA = 1

_HEX64 = re.compile(r"^[0-9a-f]{64}$")
_DOTTED = re.compile(r"^\d+(\.\d+)*$")


class Report(NamedTuple):
    """What a run found. `errors` block shipping; `unpinned` only defers it."""

    errors: list[str]
    unpinned: list[str]

    @property
    def exit_code(self) -> int:
        if self.errors:
            return 1
        if self.unpinned:
            return 2
        return 0

    def lines(self) -> list[str]:
        """One line per finding, in a stable order, for a human or a CI log."""
        out = [f"error: {item}" for item in self.errors]
        out += [f"unpinned: {item}" for item in self.unpinned]
        return out


def _text(value: Any) -> bool:
    return isinstance(value, str) and value.strip() != ""


def _hex_or_todo(value: Any) -> bool:
    return isinstance(value, str) and (bool(_HEX64.match(value)) or value == TODO)


def _is_todo(value: Any) -> bool:
    return isinstance(value, str) and value == TODO


def _relative_path(value: Any) -> bool:
    """A path inside the installation prefix: relative, no `..`, no drive or root."""
    if not _text(value):
        return False
    path = value.replace("\\", "/")
    if path.startswith("/") or ":" in path.split("/")[0]:
        return False
    parts = path.split("/")
    return all(part not in ("", ".", "..") for part in parts)


def check_shape(manifest: Any) -> Report:
    """Validates the manifest without touching any file on disk."""
    errors: list[str] = []
    unpinned: list[str] = []

    if not isinstance(manifest, dict):
        return Report(["the manifest is not a TOML table"], [])

    if manifest.get("schema") != SCHEMA:
        errors.append(f"schema: expected {SCHEMA}, found {manifest.get('schema')!r}")

    min_version = manifest.get("min_version")
    if not isinstance(min_version, str) or not _DOTTED.match(min_version):
        errors.append(
            f"min_version: expected dotted numbers (the contract's minimum), found {min_version!r}"
        )

    bundle = manifest.get("bundle")
    required_platforms: list[str] = []
    if not isinstance(bundle, dict):
        errors.append("bundle: missing [bundle] table")
    else:
        raw_required = bundle.get("required_platforms")
        if not isinstance(raw_required, list) or not raw_required:
            errors.append("bundle.required_platforms: expected a non-empty list of platform ids")
        else:
            for platform_id in raw_required:
                if isinstance(platform_id, str) and platform_id.strip():
                    required_platforms.append(platform_id)
                else:
                    errors.append(
                        f"bundle.required_platforms: {platform_id!r} is not a platform id"
                    )
        prune = bundle.get("prune", [])
        if not isinstance(prune, list) or any(not _text(entry) for entry in prune):
            errors.append("bundle.prune: expected a list of glob strings")

    platforms = manifest.get("platform")
    if not isinstance(platforms, list) or not platforms:
        return Report(errors + ["platform: expected at least one [[platform]] table"], unpinned)

    seen_ids: list[str] = []
    for index, platform in enumerate(platforms):
        where = f"platform[{index}]"
        if not isinstance(platform, dict):
            errors.append(f"{where}: not a table")
            continue
        platform_id = platform.get("id")
        if not _text(platform_id):
            errors.append(f"{where}.id: missing")
        elif not isinstance(platform_id, str):
            errors.append(f"{where}.id: {platform_id!r} is not a platform id")
        elif platform_id in seen_ids:
            errors.append(f"{where}.id: {platform_id!r} appears twice")
        else:
            seen_ids.append(platform_id)
            where = f"platform[{platform_id}]"
        for key in ("os", "arch", "install_prefix"):
            if not _text(platform.get(key)):
                errors.append(f"{where}.{key}: missing")

        components = platform.get("component")
        component_names: list[str] = []
        if not isinstance(components, list) or not components:
            errors.append(f"{where}.component: expected at least one [[platform.component]]")
        else:
            for cindex, component in enumerate(components):
                cwhere = f"{where}.component[{cindex}]"
                if not isinstance(component, dict):
                    errors.append(f"{cwhere}: not a table")
                    continue
                name = component.get("name")
                if not _text(name):
                    errors.append(f"{cwhere}.name: missing")
                elif not isinstance(name, str):
                    errors.append(f"{cwhere}.name: {name!r} is not a name")
                elif name in component_names:
                    errors.append(f"{cwhere}.name: {name!r} appears twice")
                else:
                    component_names.append(name)
                for key in ("version", "upstream", "url", "archive", "extract_prefix"):
                    if not _text(component.get(key)):
                        errors.append(f"{cwhere}.{key}: missing")
                url = component.get("url")
                if _text(url) and not str(url).startswith("https://"):
                    errors.append(f"{cwhere}.url: expected an https URL, found {url!r}")
                size = component.get("size_bytes")
                if not isinstance(size, int) or size <= 0:
                    errors.append(f"{cwhere}.size_bytes: expected a positive integer")
                sha = component.get("sha256")
                if not _hex_or_todo(sha):
                    errors.append(f"{cwhere}.sha256: expected 64 lower-case hex digits or {TODO!r}")
                elif _is_todo(sha):
                    unpinned.append(f"{cwhere}.sha256 ({component.get('name')}) is not pinned")

        files = platform.get("file")
        if not isinstance(files, list) or not files:
            errors.append(f"{where}.file: expected at least one [[platform.file]]")
            continue
        paths: list[str] = []
        for findex, entry in enumerate(files):
            fwhere = f"{where}.file[{findex}]"
            if not isinstance(entry, dict):
                errors.append(f"{fwhere}: not a table")
                continue
            path = entry.get("path")
            if not _relative_path(path):
                errors.append(f"{fwhere}.path: {path!r} is not a relative path inside the prefix")
            elif path in paths:
                errors.append(f"{fwhere}.path: {path!r} appears twice")
            else:
                paths.append(path)
                fwhere = f"{where}.file[{path}]"
            role = entry.get("role")
            if role not in ROLES:
                errors.append(f"{fwhere}.role: {role!r} is not one of {sorted(ROLES)}")
            component = entry.get("component")
            if component not in component_names:
                errors.append(f"{fwhere}.component: {component!r} names no component of {where}")
            size = entry.get("size_bytes")
            if not isinstance(size, int) or size <= 0:
                errors.append(f"{fwhere}.size_bytes: expected a positive integer")
            sha = entry.get("sha256")
            if not _hex_or_todo(sha):
                errors.append(f"{fwhere}.sha256: expected 64 lower-case hex digits or {TODO!r}")
            elif _is_todo(sha):
                unpinned.append(f"{fwhere}.sha256 is not pinned")
            source = entry.get("source")
            if source is not None and not _relative_path(source):
                errors.append(f"{fwhere}.source: {source!r} is not a relative path")

    for required in required_platforms:
        if required not in seen_ids:
            errors.append(
                f"platform: required platform {required!r} is missing from the manifest"
            )

    return Report(errors, unpinned)


def select_platform(manifest: dict[str, Any], wanted: str | None) -> tuple[dict[str, Any] | None, list[str]]:
    """Picks the platform to check on disk; `None` plus an error when it is ambiguous."""
    platforms = manifest.get("platform") or []
    if wanted is not None:
        for platform in platforms:
            if isinstance(platform, dict) and platform.get("id") == wanted:
                return platform, []
        return None, [f"--platform: {wanted!r} is not a platform in the manifest"]
    if len(platforms) == 1:
        return platforms[0], []
    return None, [
        "--files: the manifest declares more than one platform; pass --platform <id>"
    ]


def check_files(platform: dict[str, Any], root: Path) -> Report:
    """Verifies every declared file of `platform` under `root` (an unpacked prefix)."""
    errors: list[str] = []
    unpinned: list[str] = []
    files = platform.get("file")
    if not isinstance(files, list):
        return Report(["the platform declares no [[platform.file]] entries"], [])
    for entry in files:
        if not isinstance(entry, dict) or not _relative_path(entry.get("path")):
            continue  # the shape check already reported it
        path = str(entry["path"]).replace("\\", "/")
        target = root.joinpath(*path.split("/"))
        if not target.is_file():
            errors.append(f"missing: {path} is not in {root}")
            continue
        data = target.read_bytes()
        expected_size = entry.get("size_bytes")
        if isinstance(expected_size, int) and len(data) != expected_size:
            errors.append(f"size: {path} is {len(data)} bytes, the manifest says {expected_size}")
        expected = entry.get("sha256")
        if _is_todo(expected):
            unpinned.append(f"{path} has no pinned SHA-256 (presence and size only)")
            continue
        if not _hex64_if_str(expected):
            continue  # the shape check already reported it
        digest = hashlib.sha256(data).hexdigest()
        if digest != expected:
            errors.append(f"mismatch: {path} has sha256 {digest}, the manifest says {expected}")
    return Report(errors, unpinned)


def _hex64_if_str(value: Any) -> bool:
    return isinstance(value, str) and bool(_HEX64.match(value))


def load_manifest(path: Path) -> tuple[dict[str, Any] | None, str | None]:
    """Reads the TOML; returns the table or one error string."""
    if tomllib is None:
        return None, "python3.11 or newer is required (tomllib); this interpreter is older"
    try:
        with path.open("rb") as handle:
            return tomllib.load(handle), None
    except FileNotFoundError:
        return None, f"{path}: no such file"
    except OSError as error:
        return None, f"{path}: {error}"
    except tomllib.TOMLDecodeError as error:
        return None, f"{path}: not valid TOML: {error}"


def verify(manifest_path: Path, files_dir: Path | None = None, platform_id: str | None = None) -> Report:
    """The whole check: shape always, hashes when `files_dir` is given."""
    manifest, problem = load_manifest(manifest_path)
    if manifest is None:
        return Report([problem or "the manifest could not be read"], [])
    shape = check_shape(manifest)
    report = Report(list(shape.errors), list(shape.unpinned))
    if files_dir is None:
        return report
    if shape.errors:
        # A malformed manifest cannot be checked on disk meaningfully.
        return report
    platform, selection_errors = select_platform(manifest, platform_id)
    if platform is None:
        return Report(report.errors + selection_errors, report.unpinned)
    if not files_dir.is_dir():
        return Report(report.errors + [f"--files: {files_dir} is not a directory"], report.unpinned)
    on_disk = check_files(platform, files_dir)
    return Report(report.errors + on_disk.errors, report.unpinned + on_disk.unpinned)


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--manifest", required=True, type=Path, help="path to MANIFEST.toml")
    parser.add_argument(
        "--files",
        type=Path,
        default=None,
        help="an unpacked installation prefix to verify the declared files against",
    )
    parser.add_argument(
        "--platform",
        default=None,
        help="the platform id to verify with --files (required when the manifest has several)",
    )
    args = parser.parse_args(argv)

    report = verify(args.manifest, args.files, args.platform)
    for line in report.lines():
        print(line)
    if report.exit_code == 0:
        print(f"ok: {args.manifest} is valid" + ("" if args.files is None else f", and {args.files} matches"))
    elif report.exit_code == 2:
        print("not shippable: replace every 'pin before release' value before a release")
    return report.exit_code


if __name__ == "__main__":
    sys.exit(main())
