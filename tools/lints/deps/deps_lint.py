#!/usr/bin/env python3
"""Enforce G4 app-crate dependency boundaries."""

from __future__ import annotations

import argparse
import json
import subprocess
import sys
import tomllib
from pathlib import Path


HERE = Path(__file__).resolve().parent


def load_rules() -> tuple[dict[str, str], set[str]]:
    with (HERE / "rules.toml").open("rb") as stream:
        rules = tomllib.load(stream)
    return rules["classes"], set(rules["allowed"]["crate_names"])


def crate_class(manifest_path: str, root: Path, classes: dict[str, str]) -> str | None:
    try:
        relative = Path(manifest_path).resolve().relative_to(root.resolve()).as_posix()
    except ValueError:
        return None
    for category, prefix in classes.items():
        if relative.startswith(prefix.rstrip("/") + "/"):
            return category
    return None


def is_within(path: Path, parent: Path) -> bool:
    try:
        path.resolve().relative_to(parent.resolve())
        return True
    except ValueError:
        return False


def run(root: Path) -> int:
    if not (root / "Cargo.toml").is_file():
        print("G4: no Cargo workspace yet, nothing to check")
        return 0

    try:
        result = subprocess.run(
            ["cargo", "metadata", "--format-version", "1", "--no-deps", "--manifest-path", str(root / "Cargo.toml")],
            check=True,
            capture_output=True,
            text=True,
        )
    except FileNotFoundError:
        print("G4: cargo is not installed, cannot check dependencies", file=sys.stderr)
        return 2
    except subprocess.CalledProcessError as exc:
        print(exc.stderr, end="", file=sys.stderr)
        return exc.returncode or 1

    metadata = json.loads(result.stdout)
    classes, allowed_names = load_rules()
    packages = metadata["packages"]
    workspace_ids = set(metadata.get("workspace_members", []))
    package_classes = {
        package["id"]: crate_class(package["manifest_path"], root, classes)
        for package in packages
        if package["id"] in workspace_ids
    }
    names_by_id = {package["id"]: package["name"] for package in packages}
    ids_by_name: dict[str, list[str]] = {}
    for package_id, name in names_by_id.items():
        ids_by_name.setdefault(name, []).append(package_id)

    violations: set[tuple[str, str, str]] = set()
    for package in packages:
        if package["id"] not in workspace_ids or package_classes.get(package["id"]) != "app":
            continue
        app_name = package["name"]
        for dependency in package.get("dependencies", []):
            name = dependency["name"]
            if name in allowed_names:
                continue
            kind = dependency.get("kind") or "normal"
            # Only workspace crates are subject to this rule. Cargo metadata --no-deps
            # still lists local workspace dependencies with their package IDs.
            target_classes = {
                package_classes[package_id]
                for package_id in ids_by_name.get(name, [])
                if package_id in package_classes and package_classes[package_id]
            }
            if target_classes & {"framework", "backend"}:
                violations.add((app_name, name, kind))
            dependency_path = dependency.get("path")
            if dependency_path and not is_within(Path(dependency_path), root / classes["app"]):
                violations.add((app_name, name, kind))

    for app_name, crate_name, kind in sorted(violations):
        print(f"G4: {app_name} depends on {crate_name} ({kind})")
    return 1 if violations else 0


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("workspace", nargs="?", type=Path, default=HERE.parents[2])
    args = parser.parse_args()
    return run(args.workspace.resolve())


if __name__ == "__main__":
    raise SystemExit(main())
