#!/usr/bin/env python3
"""Release checks, part one (the project process notes, sections 2 and 4).

Subcommands:
  licence    licence and copyright-holder lint of the packaging inputs
  version    release tag against Cargo.toml and the package version spellings
  checksums  write SHA256SUMS in `sha256sum -c` format

Exit 0: success. Exit 1: findings (every one printed, `path:line: message`).
Exit 2: usage or environment error. Fails closed: an input that cannot be
read or decided is a finding or an error, never a pass. Standard library
only, no network, no shell.
"""
from __future__ import annotations

import argparse
import hashlib
import importlib.util
import os
import re
import subprocess
import sys
import tomllib
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[2]
TAG_RE = re.compile(r"v\d+\.\d+\.\d+(-[0-9A-Za-z]+(\.[0-9A-Za-z]+)*)?", re.ASCII)
LICENCE_ID = "Apache-2.0"
SUMS_NAME = "SHA256SUMS"

# Findings under these root-relative prefixes are ignored (seeded bad text).
ALLOWLIST = ("tools/release/tests/",)
# Verbatim third-party notices (GPL text, upstream copyright lines) that the
# desktop installer must carry unmodified: not Spotledger-authored inputs.
THIRD_PARTY_PREFIXES = ("packaging/postgis/NOTICE.md",)  # exact file only: the rest of the directory is scanned
SKIP_DIRS = {".git", "target", "node_modules", "__pycache__", "shared-target"}

BAD_TEXT = (
    (re.compile(r"LicenseRef-Proprietary|Proprietary", re.I), "proprietary licence text"),
    (re.compile(r"All rights reserved", re.I), "'All rights reserved'"),
)
LICENSE_FIELD_RE = re.compile(r"^\s*License:\s*(.*?)\s*$", re.I)
LICENSES_ATTR_RE = re.compile(r"licenses=\s*[\"']?([^\"'\s\\]*)", re.I)
COPYRIGHT_RE = re.compile(
    r"\bCopyright\b:?\s*(?:\(c\)|©)?\s*(\d[\d,\s\-–]*?)\s+(\S.*?)\s*$")


class EnvError(Exception):
    """Usage or environment problem (exit 2)."""


def _rel(root: Path, path: Path) -> str:
    return path.relative_to(root).as_posix()


def _allowed(rel: str) -> bool:
    return any(rel.startswith(p) for p in ALLOWLIST)


def _walk(root: Path):
    def _fail(exc):  # an unreadable directory must not be skipped silently
        raise EnvError(f"cannot read directory: {exc}")

    for dirpath, dirnames, filenames in os.walk(root, onerror=_fail):
        dirnames[:] = sorted(d for d in dirnames if d not in SKIP_DIRS)
        for name in sorted(filenames):
            yield Path(dirpath) / name


def _read_text(path: Path):
    """Text of a file, or None when it is binary."""
    data = path.read_bytes()
    if b"\0" in data:
        return None
    return data.decode("utf-8", errors="replace")


def holder_of(text: str):
    """The copyright holder (text after the year) of the first such line."""
    for line in text.splitlines():
        m = COPYRIGHT_RE.search(line)
        if m:
            return m.group(2).strip()
    return None


def scan_files(root: Path):
    """The files the licence check reads, root-relative, sorted, de-duplicated."""
    found = set()
    for path in _walk(root):
        rel = _rel(root, path)
        if _allowed(rel) or not path.is_file():
            continue
        name = path.name
        if rel.startswith("packaging/") and not rel.startswith(THIRD_PARTY_PREFIXES):
            found.add(rel)
        elif (rel in ("tools/packaging/build.py", "NOTICE", "Containerfile")
              or name == "Cargo.toml" or name == "Containerfile" or name.endswith(".spec")
              or rel.endswith("debian/copyright")):
            found.add(rel)
    return sorted(found)


def check_licence(root: Path):
    findings = []
    notice = root / "NOTICE"
    holder = None
    if not notice.is_file():
        findings.append("NOTICE:1: root NOTICE is missing (it names the copyright holder)")
    else:
        holder = holder_of(_read_text(notice) or "")
        if holder is None:
            findings.append("NOTICE:1: no 'Copyright <year> <holder>' line found")
    for rel in scan_files(root):
        path = root / rel
        try:
            text = _read_text(path)
        except OSError as exc:
            findings.append(f"{rel}:1: cannot read: {exc}")
            continue
        if text is None:
            continue
        for no, line in enumerate(text.splitlines(), 1):
            for rx, what in BAD_TEXT:
                if rx.search(line):
                    findings.append(f"{rel}:{no}: {what}: {line.strip()[:120]}")
            m = LICENSE_FIELD_RE.match(line)
            if m and m.group(1) != LICENCE_ID:
                findings.append(f"{rel}:{no}: License value {m.group(1)!r} is not {LICENCE_ID}")
            for m in LICENSES_ATTR_RE.finditer(line):
                if m.group(1) != LICENCE_ID:
                    findings.append(f"{rel}:{no}: licenses value {m.group(1)!r} is not {LICENCE_ID}")
            m = COPYRIGHT_RE.search(line)
            if m and holder is not None and m.group(2).strip() != holder:
                findings.append(
                    f"{rel}:{no}: stale copyright holder {m.group(2).strip()!r}, NOTICE says {holder!r}")
        if path.name == "Cargo.toml":
            findings.extend(_cargo_licence(rel, text))
    return findings


def _cargo_licence(rel: str, text: str):
    try:
        data = tomllib.loads(text)
    except tomllib.TOMLDecodeError as exc:
        return [f"{rel}:1: cannot parse Cargo.toml: {exc}"]
    out = []
    for where, tbl in (("package", data.get("package")),
                       ("workspace.package", (data.get("workspace") or {}).get("package"))):
        if isinstance(tbl, dict) and "license" in tbl:
            value = tbl["license"]
            if value == {"workspace": True}:
                continue  # inherits the [workspace.package] value, checked itself
            if value != LICENCE_ID:
                line = next((n for n, ln in enumerate(text.splitlines(), 1)
                             if re.match(r"\s*license\s*=", ln)), 1)
                out.append(f"{rel}:{line}: [{where}] license {value!r} is not {LICENCE_ID}")
    return out


def _load_build(root: Path):
    path = root / "tools" / "packaging" / "build.py"
    if not path.is_file():
        raise EnvError(f"{path} not found")
    spec = importlib.util.spec_from_file_location("spot_packaging_build", path)
    mod = importlib.util.module_from_spec(spec)
    try:
        spec.loader.exec_module(mod)
    except SystemExit as exc:
        raise EnvError(f"cannot load {path}: {exc}")
    return mod


def check_version(root: Path, tag: str, binary: str | None):
    findings = []
    if not TAG_RE.fullmatch(tag):
        return [f"<tag>:0: {tag!r} is not vX.Y.Z[-pre] (needs a leading v and three numbers)"]
    version = tag[1:]
    cargo = root / "Cargo.toml"
    try:
        data = tomllib.loads(cargo.read_text(encoding="utf-8"))
    except (OSError, tomllib.TOMLDecodeError) as exc:
        raise EnvError(f"cannot read {cargo}: {exc}")
    declared = ((data.get("workspace") or {}).get("package") or {}).get("version")
    if declared is None:
        findings.append("Cargo.toml:1: no [workspace.package] version")
    elif declared != version:
        findings.append(f"Cargo.toml:1: [workspace.package] version {declared!r} != tag {tag!r}")
    build = _load_build(root)
    try:
        spellings = build.package_versions(version)
    except SystemExit as exc:
        findings.append(f"tools/packaging/build.py:1: refuses version {version!r}: {exc}")
        spellings = None
    if spellings:
        for key in sorted(spellings):
            print(f"{key}: {spellings[key]}")
    if binary is not None:
        try:
            proc = subprocess.run([binary, "--version"], capture_output=True, text=True, timeout=60)
        except (OSError, subprocess.TimeoutExpired) as exc:
            raise EnvError(f"cannot run {binary} --version: {exc}")
        out = (proc.stdout + proc.stderr).strip()
        if not out:
            print("SKIP: binary printed no version output")
        elif version not in out:
            findings.append(f"{binary}:0: --version output {out[:120]!r} does not contain {version!r}")
    return findings


def write_checksums(directory: Path, out: Path | None):
    if not directory.is_dir():
        raise EnvError(f"{directory} is not a directory")
    out = out if out is not None else directory / SUMS_NAME
    try:
        out_abs = out.resolve()
    except OSError as exc:
        raise EnvError(f"bad output path {out}: {exc}")
    findings, names = [], []
    for entry in sorted(directory.iterdir(), key=lambda p: p.name.encode()):
        if entry.name == SUMS_NAME or entry.resolve() == out_abs:
            continue
        if entry.is_symlink() or entry.is_dir() or not entry.is_file():
            findings.append(f"{entry.name}:0: not a plain file (subdirectory or link)")
        elif "\n" in entry.name or "\\" in entry.name or "\r" in entry.name:
            findings.append(f"{entry.name!r}:0: name cannot be written in sha256sum format")
        else:
            names.append(entry)
    if not findings and not names:
        findings.append(f"{directory}:0: no files to checksum")
    if findings:
        return findings
    lines = []
    for entry in names:
        h = hashlib.sha256()
        with entry.open("rb") as fh:
            for chunk in iter(lambda: fh.read(1 << 20), b""):
                h.update(chunk)
        lines.append(f"{h.hexdigest()}  {entry.name}\n")
    out.write_bytes("".join(lines).encode("utf-8"))
    print(f"wrote {out} ({len(lines)} files)")
    return []


def build_parser():
    p = argparse.ArgumentParser(description="Release checks, part one.")
    sub = p.add_subparsers(dest="cmd", required=True)
    a = sub.add_parser("licence", help="licence and copyright-holder lint")
    a.add_argument("--root", type=Path, default=REPO_ROOT)
    b = sub.add_parser("version", help="tag against Cargo.toml and package versions")
    b.add_argument("--tag", required=True)
    b.add_argument("--root", type=Path, default=REPO_ROOT)
    b.add_argument("--binary")
    c = sub.add_parser("checksums", help="write SHA256SUMS")
    c.add_argument("--dir", required=True, type=Path)
    c.add_argument("--out", type=Path)
    return p


def main(argv=None):
    args = build_parser().parse_args(argv)
    try:
        if args.cmd == "licence":
            findings = check_licence(args.root.resolve())
        elif args.cmd == "version":
            findings = check_version(args.root.resolve(), args.tag, args.binary)
        else:
            findings = write_checksums(args.dir, args.out)
    except EnvError as exc:
        print(f"error: {exc}", file=sys.stderr)
        return 2
    for f in findings:
        print(f)
    return 1 if findings else 0


if __name__ == "__main__":
    sys.exit(main())
