#!/usr/bin/env python3
"""Verify a downloaded Spotledger release folder (the project process notes).

    verify_download.py DIR --tag vX.Y.Z[-pre] [--commit SHA] [--require-signed]

Works from a plain download folder: no git checkout, standard library only, no
network. Nothing in DIR is executed or extracted; the tarball is only listed.

Checks (every finding is printed, `path: message`):
  * SHA256SUMS exists, is well formed, names plain files only (no path
    separators, no `..`, no duplicates) and every line matches the file's real
    SHA-256;
  * every file in DIR is listed and every listed file is present (SHA256SUMS
    itself, and detached signature files, excepted);
  * exactly one tarball, one .deb, one .rpm and one .cdx.json, each named for
    the tag's version;
  * the tarball holds LICENSE and NOTICE;
  * the SBOM is JSON with bomFormat "CycloneDX" and at least one component;
  * with --commit, RELEASE-NOTES.md carries a SOURCE-COMMIT line equal to it;
  * with --require-signed, a detached signature of SHA256SUMS (SHA256SUMS.asc or
    SHA256SUMS.sig) is present and `gpg --verify` accepts it (releases are
    unsigned in 0.x, so this is off by default).

Exit 0: pass. Exit 1: any finding. Exit 2: usage or environment error.
Fails closed: anything that cannot be read or decided is a finding.
"""
from __future__ import annotations

import argparse
import hashlib
import json
import re
import shutil
import subprocess
import sys
import tarfile
from pathlib import Path

SUMS_NAME = "SHA256SUMS"
NOTES_NAME = "RELEASE-NOTES.md"
SIG_NAMES = (SUMS_NAME + ".asc", SUMS_NAME + ".sig")
TAG_RE = re.compile(r"v\d+\.\d+\.\d+(-[0-9A-Za-z]+(\.[0-9A-Za-z]+)*)?", re.ASCII)
SUM_LINE_RE = re.compile(r"([0-9a-fA-F]{64}) [ *](.+)", re.ASCII)
COMMIT_RE = re.compile(r"[0-9a-f]{40}", re.ASCII)
# `SOURCE-COMMIT: <sha>` (also `SOURCE-COMMIT=<sha>`), optionally in backticks.
SOURCE_COMMIT_RE = re.compile(r"^SOURCE-COMMIT\s*[:=]\s*`?([0-9A-Za-z]+)`?\s*$", re.M)
MAX_SBOM_BYTES = 64 << 20
MAX_NOTES_BYTES = 1 << 20


class UsageError(Exception):
    """Usage or environment problem (exit 2)."""


def sha256_of(path: Path) -> str:
    h = hashlib.sha256()
    with path.open("rb") as fh:
        for chunk in iter(lambda: fh.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


def safe_name(name: str) -> str | None:
    """Why `name` is not a plain file name in the download folder, or None if it is."""
    if not name or name in (".", "..") or name != name.strip():
        return "empty or padded file name"
    if "/" in name or "\\" in name:
        return "path separator in file name"
    if ".." in name:
        return "'..' in file name"
    if name.startswith("-"):
        return "file name starts with '-'"
    if any(ord(c) < 32 or ord(c) == 127 for c in name):
        return "control character in file name"
    if len(name) >= 2 and name[1] == ":" and name[0].isalpha():
        return "drive prefix in file name"
    return None


def spellings(version: str) -> dict[str, re.Pattern]:
    """File name patterns for a version (see tools/packaging/build.py naming)."""
    base, _, pre = version.partition("-")
    v, b = re.escape(version), re.escape(base)
    if pre:
        deb_v = re.escape(f"{base}~{pre}")
        rpm_rel = re.escape("0." + pre.replace("-", "."))
    else:
        deb_v = b
        rpm_rel = r"1"
    return {
        "tarball": re.compile(rf"spot-{v}-linux-[A-Za-z0-9_]+\.tar\.gz"),
        "deb": re.compile(rf"spot_{deb_v}_[A-Za-z0-9]+\.deb"),
        "rpm": re.compile(rf"spot-{b}-{rpm_rel}(\.[A-Za-z0-9_]+)*\.rpm"),
        "sbom": re.compile(rf"spot-{v}\.cdx\.json"),
    }


def parse_sums(sums: Path, findings: list[str]) -> dict[str, str]:
    """name -> expected hex digest; malformed or unsafe lines are findings."""
    try:
        text = sums.read_bytes().decode("utf-8")
    except (OSError, UnicodeDecodeError) as exc:
        findings.append(f"{SUMS_NAME}: unreadable: {exc}")
        return {}
    entries: dict[str, str] = {}
    lines = text.split("\n")
    if lines and lines[-1] == "":
        lines.pop()
    if not lines:
        findings.append(f"{SUMS_NAME}: empty")
    for n, line in enumerate(lines, 1):
        m = SUM_LINE_RE.fullmatch(line)
        if not m:
            findings.append(f"{SUMS_NAME}:{n}: malformed line (expected '<sha256>  <name>')")
            continue
        digest, name = m.group(1).lower(), m.group(2)
        why = safe_name(name)
        if why:
            findings.append(f"{SUMS_NAME}:{n}: refused name {name!r}: {why}")
            continue
        if name == SUMS_NAME:
            findings.append(f"{SUMS_NAME}:{n}: lists itself")
            continue
        if name in entries:
            findings.append(f"{SUMS_NAME}:{n}: {name} listed twice")
            continue
        entries[name] = digest
    return entries


def check_tarball(path: Path, findings: list[str]) -> None:
    found: set[str] = set()
    try:
        with tarfile.open(path, "r:gz") as tf:
            for member in tf:
                parts = member.name.replace("\\", "/").split("/")
                if member.name.startswith("/") or ".." in parts:
                    findings.append(f"{path.name}: unsafe member name {member.name!r}")
                    continue
                if member.isfile() and parts[-1] in ("LICENSE", "NOTICE"):
                    found.add(parts[-1])
    except (tarfile.TarError, OSError, EOFError) as exc:
        findings.append(f"{path.name}: not a readable gzip tarball: {exc}")
        return
    for need in ("LICENSE", "NOTICE"):
        if need not in found:
            findings.append(f"{path.name}: {need} missing from the tarball")


def check_sbom(path: Path, findings: list[str]) -> None:
    try:
        if path.stat().st_size > MAX_SBOM_BYTES:
            findings.append(f"{path.name}: SBOM larger than {MAX_SBOM_BYTES} bytes")
            return
        doc = json.loads(path.read_bytes().decode("utf-8"))
    except (OSError, UnicodeDecodeError, ValueError) as exc:
        findings.append(f"{path.name}: not valid JSON: {exc}")
        return
    if not isinstance(doc, dict) or doc.get("bomFormat") != "CycloneDX":
        findings.append(f"{path.name}: bomFormat is not 'CycloneDX'")
        return
    comps = doc.get("components")
    if not isinstance(comps, list) or not comps:
        findings.append(f"{path.name}: no components")


def check_commit(directory: Path, commit: str, findings: list[str]) -> None:
    notes = directory / NOTES_NAME
    try:
        if notes.stat().st_size > MAX_NOTES_BYTES:
            findings.append(f"{NOTES_NAME}: larger than {MAX_NOTES_BYTES} bytes")
            return
        text = notes.read_bytes().decode("utf-8")
    except (OSError, UnicodeDecodeError) as exc:
        findings.append(f"{NOTES_NAME}: cannot check --commit: {exc}")
        return
    found = SOURCE_COMMIT_RE.findall(text)
    if len(found) != 1:
        findings.append(f"{NOTES_NAME}: expected exactly one SOURCE-COMMIT line, found {len(found)}")
    elif found[0].lower() != commit:
        findings.append(f"{NOTES_NAME}: SOURCE-COMMIT {found[0]} is not {commit}")


def check_signature(directory: Path, findings: list[str]) -> None:
    sigs = [directory / n for n in SIG_NAMES if (directory / n).is_file()]
    if not sigs:
        findings.append(f"{SUMS_NAME}: signature required but neither {SIG_NAMES[0]} nor {SIG_NAMES[1]} is present")
        return
    gpg = shutil.which("gpg")
    if not gpg:
        findings.append(f"{SUMS_NAME}: signature required but gpg is not installed, cannot verify")
        return
    for sig in sigs:
        try:
            r = subprocess.run([gpg, "--batch", "--verify", str(sig), str(directory / SUMS_NAME)],
                               capture_output=True, text=True, timeout=60, check=False)
        except (OSError, subprocess.SubprocessError) as exc:
            findings.append(f"{sig.name}: gpg could not run: {exc}")
            continue
        if r.returncode != 0:
            findings.append(f"{sig.name}: signature does not verify")


def verify(directory: Path, tag: str, commit: str | None, require_signed: bool) -> list[str]:
    findings: list[str] = []
    version = tag[1:]
    files: dict[str, Path] = {}
    for entry in sorted(directory.iterdir(), key=lambda p: p.name):
        if entry.is_symlink() or not entry.is_file():
            findings.append(f"{entry.name}: not a plain file (subdirectory, link or special file)")
        else:
            files[entry.name] = entry

    sums_path = files.get(SUMS_NAME)
    if sums_path is None:
        findings.append(f"{SUMS_NAME}: missing")
        entries: dict[str, str] = {}
    else:
        entries = parse_sums(sums_path, findings)

    if sums_path is not None:
        for name, digest in entries.items():
            path = files.get(name)
            if path is None:
                findings.append(f"{name}: listed in {SUMS_NAME} but missing")
                continue
            try:
                actual = sha256_of(path)
            except OSError as exc:
                findings.append(f"{name}: unreadable: {exc}")
                continue
            if actual != digest:
                findings.append(f"{name}: checksum mismatch (expected {digest}, got {actual})")
        for name in files:
            if name == SUMS_NAME or name in SIG_NAMES or name in entries:
                continue
            findings.append(f"{name}: present but not listed in {SUMS_NAME}")

    pats = spellings(version)
    suffix = {"tarball": ".tar.gz", "deb": ".deb", "rpm": ".rpm", "sbom": ".cdx.json"}
    chosen: dict[str, Path] = {}
    for kind, pat in pats.items():
        matching = [n for n in files if n.endswith(suffix[kind])]
        if not matching:
            findings.append(f"{kind}: no {suffix[kind]} file in the folder")
            continue
        if len(matching) > 1:
            findings.append(f"{kind}: expected one {suffix[kind]} file, found {', '.join(matching)}")
            continue
        if not pat.fullmatch(matching[0]):
            findings.append(f"{matching[0]}: name does not match version {version} (expected {pat.pattern})")
            continue
        chosen[kind] = files[matching[0]]

    if "tarball" in chosen:
        check_tarball(chosen["tarball"], findings)
    if "sbom" in chosen:
        check_sbom(chosen["sbom"], findings)
    if commit is not None:
        check_commit(directory, commit, findings)
    if require_signed:
        check_signature(directory, findings)
    return findings


def build_parser() -> argparse.ArgumentParser:
    p = argparse.ArgumentParser(description="Verify a downloaded Spotledger release folder.")
    p.add_argument("dir", help="folder holding the downloaded release assets")
    p.add_argument("--tag", required=True, help="release tag, for example v0.1.0-rc.1")
    p.add_argument("--commit", help="full 40-character source commit the notes must name")
    p.add_argument("--require-signed", action="store_true",
                   help="also require a verifying detached signature of SHA256SUMS (off in 0.x)")
    return p


def main(argv: list[str] | None = None) -> int:
    args = build_parser().parse_args(argv)  # argparse exits 2 on a usage error
    try:
        if not TAG_RE.fullmatch(args.tag):
            raise UsageError(f"{args.tag!r} is not a release tag (vX.Y.Z or vX.Y.Z-pre)")
        commit = None
        if args.commit is not None:
            commit = args.commit.lower()
            if not COMMIT_RE.fullmatch(commit):
                raise UsageError("--commit must be a full 40-character hex commit id")
        directory = Path(args.dir)
        if not directory.is_dir():
            raise UsageError(f"{directory} is not a directory")
        findings = verify(directory, args.tag, commit, args.require_signed)
    except UsageError as exc:
        print(f"verify_download: {exc}", file=sys.stderr)
        return 2
    except OSError as exc:
        print(f"verify_download: {exc}", file=sys.stderr)
        return 2
    if findings:
        for f in findings:
            print(f)
        print(f"verify_download: FAILED with {len(findings)} finding(s)", file=sys.stderr)
        return 1
    print(f"verify_download: {args.tag} OK")
    return 0


if __name__ == "__main__":
    sys.exit(main())
