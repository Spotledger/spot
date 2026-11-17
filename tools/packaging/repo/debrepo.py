"""Assemble a signed APT repository from built ``.deb`` files, in pure Python.

Layout (one suite per channel, ``stable`` or ``beta``)::

    deb/pool/<channel>/main/<first letter>/<package>/<file>.deb
    deb/dists/<channel>/main/binary-<arch>/Packages, Packages.gz
    deb/dists/<channel>/Release, InRelease (clear-signed), Release.gpg (detached)

No apt tool is needed to build it. ``verify_repo`` is the client model: it checks
``InRelease`` against the pinned keyring, then every hash down to the ``.deb``
files, and fails on the first mismatch.
"""

from __future__ import annotations

import gzip
import hashlib
import io
import lzma
import shutil
import tarfile
import time
from pathlib import Path

from signing import Signer, verify

CHANNELS = ("stable", "beta")
AR_MAGIC = b"!<arch>\n"
CONTROL_FIELDS_SKIPPED = {"Filename", "Size", "MD5sum", "SHA1", "SHA256", "SHA512"}


class RepoError(Exception):
    """The repository cannot be built, or fails verification."""


def _decompress(name: str, data: bytes) -> bytes:
    if name.endswith(".gz"):
        return gzip.decompress(data)
    if name.endswith(".xz"):
        return lzma.decompress(data)
    if name.endswith(".zst"):
        try:
            from compression import zstd  # Python 3.14+
        except ImportError as exc:
            raise RepoError("a .zst control member needs Python 3.14 (compression.zstd)") from exc
        return zstd.decompress(data)
    return data


def read_control(deb: Path) -> list[tuple[str, str]]:
    """The control fields of a ``.deb`` as ordered ``(name, value)`` pairs (values keep continuation lines)."""
    blob = deb.read_bytes()
    if not blob.startswith(AR_MAGIC):
        raise RepoError(f"not a .deb archive: {deb.name}")
    pos = len(AR_MAGIC)
    while pos + 60 <= len(blob):
        header = blob[pos:pos + 60]
        name = header[:16].decode("ascii", "replace").strip().rstrip("/")
        try:
            size = int(header[48:58].decode("ascii").strip())
        except ValueError as exc:
            raise RepoError(f"corrupt ar header in {deb.name}") from exc
        body = blob[pos + 60:pos + 60 + size]
        pos += 60 + size + (size & 1)
        if name.startswith("control.tar"):
            tar = tarfile.open(fileobj=io.BytesIO(_decompress(name, body)))
            for member in tar.getmembers():
                if member.name in ("control", "./control"):
                    text = tar.extractfile(member).read().decode("utf-8")  # type: ignore[union-attr]
                    return parse_fields(text)
    raise RepoError(f"no control file in {deb.name}")


def parse_fields(text: str) -> list[tuple[str, str]]:
    fields: list[tuple[str, str]] = []
    for line in text.split("\n"):
        if not line.strip():
            continue
        if line[0] in " \t" and fields:
            name, value = fields[-1]
            fields[-1] = (name, value + "\n" + line)
        elif ":" in line:
            name, value = line.split(":", 1)
            fields.append((name.strip(), value.strip()))
    return fields


def _hashes(path: Path) -> dict[str, str]:
    data = path.read_bytes()
    return {"MD5sum": hashlib.md5(data, usedforsecurity=False).hexdigest(),  # index field only
            "SHA256": hashlib.sha256(data).hexdigest()}


def packages_stanza(control: list[tuple[str, str]], filename: str, deb: Path) -> str:
    """One ``Packages`` stanza: the control fields, then Filename/Size/MD5sum/SHA256 (before Description)."""
    extra = [("Filename", filename), ("Size", str(deb.stat().st_size))]
    hashes = _hashes(deb)
    extra += [("MD5sum", hashes["MD5sum"]), ("SHA256", hashes["SHA256"])]
    fields = [(n, v) for n, v in control if n not in CONTROL_FIELDS_SKIPPED]
    out: list[tuple[str, str]] = []
    placed = False
    for name, value in fields:
        if name == "Description" and not placed:
            out += extra
            placed = True
        out.append((name, value))
    if not placed:
        out += extra
    return "".join(f"{n}: {v}\n" for n, v in out)


def release_date(epoch: int) -> str:
    return time.strftime("%a, %d %b %Y %H:%M:%S UTC", time.gmtime(epoch))


def build_repo(debs: list[Path], out: Path, channel: str, signer: Signer, epoch: int,
               origin: str = "Spotledger", valid_days: int | None = None) -> Path:
    """Write the repository under ``out/deb`` and sign it. Returns ``out/deb``."""
    if channel not in CHANNELS:
        raise RepoError(f"unknown channel {channel!r}: expected one of {CHANNELS}")
    if not debs:
        raise RepoError("no .deb files given")
    root = out / "deb"
    suite = root / "dists" / channel
    if suite.exists():
        shutil.rmtree(suite)
    by_arch: dict[str, list[str]] = {}
    for deb in sorted(debs):
        control = dict(read_control(deb))
        package, arch = control.get("Package"), control.get("Architecture")
        if not package or not arch:
            raise RepoError(f"{deb.name}: control has no Package or Architecture")
        rel = f"pool/{channel}/main/{package[0]}/{package}/{deb.name}"
        dest = root / rel
        dest.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(deb, dest)
        by_arch.setdefault(arch, []).append(packages_stanza(read_control(deb), rel, dest))
    arches = sorted(a for a in by_arch if a != "all")
    if "all" in by_arch:  # architecture-independent packages are listed in every real architecture
        arches = arches or ["amd64"]
        for arch in arches:
            by_arch[arch] = by_arch.get(arch, []) + by_arch["all"]
    index_files: list[Path] = []
    for arch in arches:
        folder = suite / "main" / f"binary-{arch}"
        folder.mkdir(parents=True, exist_ok=True)
        text = "\n".join(by_arch[arch]).encode("utf-8")
        (folder / "Packages").write_bytes(text)
        (folder / "Packages.gz").write_bytes(gzip.compress(text, compresslevel=9, mtime=0))
        index_files += [folder / "Packages", folder / "Packages.gz"]
    lines = [
        f"Origin: {origin}", f"Label: {origin}", f"Suite: {channel}", f"Codename: {channel}",
        f"Date: {release_date(epoch)}",
    ]
    if valid_days is not None:
        lines.append(f"Valid-Until: {release_date(epoch + valid_days * 86400)}")
    lines += [f"Architectures: {' '.join(arches)}", "Components: main",
              f"Description: {origin} {channel} repository", "SHA256:"]
    for path in sorted(index_files):
        data = path.read_bytes()
        lines.append(f" {hashlib.sha256(data).hexdigest()} {len(data):>16} {path.relative_to(suite).as_posix()}")
    release = suite / "Release"
    release.write_text("\n".join(lines) + "\n", encoding="utf-8")
    signer.clearsign(release, suite / "InRelease")
    signer.detached(release, suite / "Release.gpg")
    return root


def _parse_release_hashes(release: str) -> dict[str, tuple[str, int]]:
    hashes: dict[str, tuple[str, int]] = {}
    in_sha = False
    for line in release.splitlines():
        if line.startswith("SHA256:"):
            in_sha = True
            continue
        if in_sha:
            if not line.startswith(" "):
                in_sha = False
                continue
            digest, size, name = line.split()
            hashes[name] = (digest, int(size))
    return hashes


def verify_repo(root: Path, channel: str, keyring: Path) -> None:
    """Client model. Raises :class:`RepoError` on any signature or hash mismatch."""
    suite = root / "dists" / channel
    inrelease = suite / "InRelease"
    if not inrelease.is_file():
        raise RepoError("no InRelease")
    if not verify(keyring, inrelease):
        raise RepoError("InRelease signature is not valid for the pinned keys")
    release = suite / "Release"
    if release.is_file() and (suite / "Release.gpg").is_file():
        if not verify(keyring, suite / "Release.gpg", release):
            raise RepoError("Release.gpg signature is not valid for the pinned keys")
    # The signed text is what counts: hashes come from the clear-signed InRelease.
    signed = inrelease.read_text(encoding="utf-8")
    body = signed.split("\n\n", 1)[1].split("-----BEGIN PGP SIGNATURE-----")[0]
    entries = _parse_release_hashes(body)
    if not entries:
        raise RepoError("Release lists no files")
    for name, (digest, size) in entries.items():
        path = suite / name
        if not path.is_file():
            raise RepoError(f"{name} is listed in Release but missing")
        data = path.read_bytes()
        if len(data) != size or hashlib.sha256(data).hexdigest() != digest:
            raise RepoError(f"{name} does not match Release")
        if name.endswith("/Packages"):
            for stanza in data.decode("utf-8").split("\n\n"):
                fields = dict(parse_fields(stanza))
                if not fields:
                    continue
                deb = root / fields["Filename"]
                if not deb.is_file():
                    raise RepoError(f"{fields['Filename']} is listed in Packages but missing")
                blob = deb.read_bytes()
                if len(blob) != int(fields["Size"]) or hashlib.sha256(blob).hexdigest() != fields["SHA256"]:
                    raise RepoError(f"{fields['Filename']} does not match Packages")
