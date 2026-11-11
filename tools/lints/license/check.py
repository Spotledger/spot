#!/usr/bin/env python3
"""Licence lint (open-source plan, phase 1): the Platform layer is Apache-2.0.

Checks:
  1. Every workspace package whose manifest lies under the root has license == "Apache-2.0",
     unless listed in allow.txt.
  2. No Cargo.toml (git-tracked, or found by a directory walk outside a git tree) contains
     LicenseRef-Proprietary, unless its path is listed in proprietary-paths.txt.

Usage: python3 tools/lints/license/check.py [--root DIR] [--metadata FILE]   (exit 1 on any finding)
"""
from __future__ import annotations

import argparse
import json
import subprocess
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
EXPECTED = "Apache-2.0"
MARKER = "LicenseRef-Proprietary"
SKIP_DIRS = {"target", ".git", "node_modules", "bin", "obj"}


def read_list(path: Path) -> set[str]:
    out = set()
    if path.is_file():
        for ln in path.read_text(encoding="utf-8").splitlines():
            ln = ln.split("#", 1)[0].strip()
            if ln:
                out.add(ln)
    return out


def package_findings(meta: dict, root: Path, allow: set[str]) -> list[str]:
    root = root.resolve()
    out = []
    for p in meta.get("packages", []):
        mp = Path(p["manifest_path"]).resolve()
        if root not in mp.parents or p["name"] in allow:
            continue
        lic = p.get("license")
        if lic != EXPECTED:
            out.append(f"{mp.relative_to(root).as_posix()}: package {p['name']} has license {lic!r}, expected {EXPECTED!r}")
    return out


def manifests(root: Path) -> list[str]:
    try:
        r = subprocess.run(["git", "-C", str(root), "ls-files", "--", "*Cargo.toml"],
                           capture_output=True, text=True, check=True)
        top = subprocess.run(["git", "-C", str(root), "rev-parse", "--show-toplevel"],
                             capture_output=True, text=True, check=True).stdout.strip()
        if Path(top).resolve() == root.resolve():
            return sorted(f for f in r.stdout.splitlines() if f.endswith("Cargo.toml"))
    except (subprocess.CalledProcessError, FileNotFoundError):
        pass
    found = []
    for p in root.rglob("Cargo.toml"):
        rel = p.relative_to(root)
        if not SKIP_DIRS.intersection(rel.parts[:-1]):
            found.append(rel.as_posix())
    return sorted(found)


def marker_findings(root: Path, allowed: set[str]) -> list[str]:
    out = []
    for rel in manifests(root):
        f = root / rel
        if rel in allowed or not f.is_file():
            continue
        if MARKER in f.read_text(encoding="utf-8", errors="replace"):
            out.append(f"{rel}: still contains {MARKER}")
    return out


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--root", type=Path, default=HERE.parents[2])
    ap.add_argument("--metadata", type=Path)
    a = ap.parse_args()
    root = a.root.resolve()
    if a.metadata:
        meta = json.loads(a.metadata.read_text(encoding="utf-8"))
    else:
        r = subprocess.run(["cargo", "metadata", "--format-version", "1", "--no-deps"],
                           cwd=root, capture_output=True, text=True)
        if r.returncode != 0:
            print(f"cargo metadata failed:\n{r.stderr}", file=sys.stderr)
            return 1
        meta = json.loads(r.stdout)
    found = package_findings(meta, root, read_list(HERE / "allow.txt"))
    found += marker_findings(root, read_list(HERE / "proprietary-paths.txt"))
    for f in found:
        print(f)
    if found:
        print(f"licence lint: {len(found)} finding(s)")
        return 1
    print("licence lint: ok")
    return 0


if __name__ == "__main__":
    sys.exit(main())
