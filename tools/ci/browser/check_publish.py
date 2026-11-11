#!/usr/bin/env python3
"""Verify a published browser build (WP-UI-09.T4).

    python3 tools/ci/browser/check_publish.py <publish folder> [--report out.json]

The folder is the web root of `dotnet publish clients/avalonia/src/Spotledger.Client.Browser`
(the one that holds `assets.json`). The checks follow `clients/avalonia/docs/browser-publish.md`:

* `assets.json` exists, has version 1 and a 64-hex `build`;
* every listed file exists with the stated size and SHA-256;
* no published file is unlisted (the manifest does not list itself);
* the `build` id is the documented digest over the listed paths and hashes;
* the Brotli siblings exist (every compressible file has one, and there is at least one);
* the total compressed size is printed and, when `tools/browser-budgets/budgets.toml` carries
  `browser.download.compressed`, compared with it;
* the linker-roots file named by the T1 brief is present.

Pure standard library, no network. Exit 0 when every check passes, 1 otherwise, 2 on bad usage.
"""
from __future__ import annotations

import argparse
import hashlib
import json
import sys
import tomllib
from pathlib import Path

REPO = Path(__file__).resolve().parents[3]
DEFAULT_BUDGETS = REPO / "tools" / "browser-budgets" / "budgets.toml"
DEFAULT_LINKER_ROOTS = REPO / "clients" / "avalonia" / "src" / "Spotledger.Client.Browser" / "linker-roots.xml"
BUDGET_ID = "browser.download.compressed"
MANIFEST = "assets.json"
COMPRESSED_SUFFIXES = (".br", ".gz")
# Files the publish pre-compresses; each must have a Brotli sibling.
COMPRESSIBLE = (".wasm", ".js", ".mjs", ".dll", ".dat", ".css")


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1 << 20), b""):
            digest.update(chunk)
    return digest.hexdigest()


def build_id(files) -> str:
    """SHA-256 over every path and hash, in path order (Build/AssetManifest.cs BuildId: path NUL hash LF)."""
    digest = hashlib.sha256()
    for entry in sorted(files, key=lambda e: e["path"]):
        digest.update(f"{entry['path']}\0{entry['sha256']}\n".encode())
    return digest.hexdigest()


def is_hex64(value) -> bool:
    return isinstance(value, str) and len(value) == 64 and all(c in "0123456789abcdef" for c in value)


def load_budget(path: Path | None):
    """The `browser.download.compressed` budget as a dict, or None when absent."""
    if path is None or not path.is_file():
        return None
    data = tomllib.loads(path.read_text(encoding="utf-8"))
    for entry in data.get("budget", []):
        if entry.get("id") == BUDGET_ID:
            return entry
    return None


def check(root: Path, budgets: Path | None, linker_roots: Path | None):
    """Return (errors, report). Errors is a list of strings; empty means pass."""
    errors: list[str] = []
    report: dict = {"root": str(root)}
    if not root.is_dir():
        return [f"publish folder not found: {root}"], report

    manifest_path = root / MANIFEST
    if not manifest_path.is_file():
        return [f"{MANIFEST} is missing in {root}"], report
    try:
        manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
    except (OSError, ValueError) as exc:
        return [f"{MANIFEST} is not valid JSON: {exc}"], report
    if not isinstance(manifest, dict) or manifest.get("version") != 1:
        errors.append(f"{MANIFEST}: version must be 1")
    listed = manifest.get("files") if isinstance(manifest, dict) else None
    if not isinstance(listed, list) or not listed:
        return errors + [f"{MANIFEST}: 'files' must be a non-empty list"], report
    if not is_hex64(manifest.get("build")):
        errors.append(f"{MANIFEST}: 'build' must be 64 lowercase hex characters")

    entries: dict[str, dict] = {}
    for index, entry in enumerate(listed):
        ok = (isinstance(entry, dict) and isinstance(entry.get("path"), str)
              and isinstance(entry.get("bytes"), int) and not isinstance(entry.get("bytes"), bool)
              and is_hex64(entry.get("sha256")))
        if not ok:
            errors.append(f"{MANIFEST}: files[{index}] needs path, bytes and a 64-hex sha256")
            continue
        path = entry["path"]
        if path == MANIFEST:
            errors.append(f"{MANIFEST}: must not list itself")
        if path.startswith("/") or ".." in path.split("/"):
            errors.append(f"{MANIFEST}: unsafe path {path!r}")
            continue
        if path in entries:
            errors.append(f"{MANIFEST}: path listed twice: {path}")
            continue
        entries[path] = entry

    paths = [e["path"] for e in listed if isinstance(e, dict) and isinstance(e.get("path"), str)]
    if paths != sorted(paths):
        errors.append(f"{MANIFEST}: files are not sorted by path")

    # Every listed file exists with the stated size and hash.
    for path, entry in sorted(entries.items()):
        target = root / path
        if not target.is_file():
            errors.append(f"listed file is missing: {path}")
            continue
        size = target.stat().st_size
        if size != entry["bytes"]:
            errors.append(f"size differs for {path}: manifest {entry['bytes']}, file {size}")
        actual = sha256_file(target)
        if actual != entry["sha256"]:
            errors.append(f"hash differs for {path}: manifest {entry['sha256']}, file {actual}")

    # No published file is unlisted.
    on_disk = sorted(p.relative_to(root).as_posix() for p in root.rglob("*") if p.is_file())
    unlisted = [p for p in on_disk if p != MANIFEST and p not in entries]
    for path in unlisted:
        errors.append(f"published file is not listed in {MANIFEST}: {path}")

    if is_hex64(manifest.get("build")) and not errors:
        expected = build_id(entries.values())
        if expected != manifest["build"]:
            errors.append(f"'build' does not match the listed paths and hashes (expected {expected})")

    # Brotli siblings.
    names = set(on_disk)
    brotli = [p for p in on_disk if p.endswith(".br")]
    if not brotli:
        errors.append("no .br file in the publish folder (Brotli pre-compression is missing)")
    missing_br = [p for p in on_disk
                  if p.lower().endswith(COMPRESSIBLE) and p + ".br" not in names]
    for path in missing_br:
        errors.append(f"no Brotli sibling for {path}")

    # Total compressed size: the .br sibling where there is one, else the file itself;
    # the .br and .gz files are the encodings of the file they sit next to, not extra files.
    total = 0
    for path in on_disk:
        if path.endswith(COMPRESSED_SUFFIXES) and path.rsplit(".", 1)[0] in names:
            continue
        sibling = root / (path + ".br")
        total += (sibling if sibling.is_file() else root / path).stat().st_size
    report.update(files=len(on_disk), listed=len(entries), brotli_files=len(brotli),
                  compressed_total_bytes=total, build=manifest.get("build"))
    print(f"compressed total: {total} bytes ({total / 1048576:.2f} MiB) over {len(on_disk)} files")

    budget = load_budget(budgets)
    if budget is None:
        report["budget"] = None
        print(f"no budget {BUDGET_ID} in {budgets}; size not compared")
    else:
        limit = budget.get("max")
        report["budget"] = {"id": BUDGET_ID, "max": limit}
        if not isinstance(limit, (int, float)) or isinstance(limit, bool):
            errors.append(f"budget {BUDGET_ID} has no numeric max")
        else:
            verdict = "within" if total <= limit else "OVER"
            print(f"budget {BUDGET_ID}: {total} of {limit} bytes ({verdict})")
            if total > limit:
                errors.append(f"compressed size {total} exceeds budget {BUDGET_ID} max {limit}")

    if linker_roots is not None:
        report["linker_roots"] = str(linker_roots)
        if not linker_roots.is_file():
            errors.append(f"linker-roots file is missing: {linker_roots}")
    report["errors"] = errors
    return errors, report


def main(argv=None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("folder", type=Path, help="the published web root (holds assets.json)")
    parser.add_argument("--budgets", type=Path, default=DEFAULT_BUDGETS)
    parser.add_argument("--linker-roots", type=Path, default=DEFAULT_LINKER_ROOTS)
    parser.add_argument("--report", type=Path, help="write the size report JSON here")
    args = parser.parse_args(argv)

    errors, report = check(args.folder, args.budgets, args.linker_roots)
    if args.report:
        args.report.parent.mkdir(parents=True, exist_ok=True)
        args.report.write_text(json.dumps(report, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    for message in errors:
        print(f"ERROR: {message}", file=sys.stderr)
    print("browser publish check: " + ("FAILED" if errors else "ok"))
    return 1 if errors else 0


if __name__ == "__main__":
    sys.exit(main())
