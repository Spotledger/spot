"""Assemble a signed RPM repository from built ``.rpm`` files.

Layout (one directory per channel)::

    rpm/<channel>/Packages/<file>.rpm        each signed with rpmsign
    rpm/<channel>/repodata/...               createrepo_c output
    rpm/<channel>/repodata/repomd.xml.asc    detached signature of repomd.xml

``rpmsign`` and ``createrepo_c`` do the work and must be installed: a missing
tool is an error, never a silently unsigned repository. ``verify_repo`` is the
client model for ``repo_gpgcheck=1`` (the metadata signature, then every hash of
``repomd.xml`` and ``primary``) and needs no rpm tool.
"""

from __future__ import annotations

import gzip
import hashlib
import lzma
import shutil
import subprocess
import tempfile
import xml.etree.ElementTree as ET
from pathlib import Path

from signing import Signer, SigningError, need, verify

CHANNELS = ("stable", "beta")
REPO_NS = "{http://linux.duke.edu/metadata/repo}"
COMMON_NS = "{http://linux.duke.edu/metadata/common}"


class RepoError(Exception):
    """The repository cannot be built, or fails verification."""


def sign_package(rpm: Path, signer: Signer) -> None:
    """``rpmsign --addsign`` with the signer's key, passphrase from the file (never argv)."""
    rpmsign = need("rpmsign")
    sign_cmd = (
        "%{__gpg} gpg --batch --no-tty --no-verbose --no-armor --no-secmem-warning "
        "--pinentry-mode loopback --digest-algo SHA256 "
        + (f"--passphrase-file {signer.passphrase_file} " if signer.passphrase_file else "")
        + "-u %{_gpg_name} -sbo %{__signature_filename} %{__plaintext_filename}"
    )
    cmd = [rpmsign, "--define", f"_gpg_path {signer.home}", "--define", f"_gpg_name {signer.fingerprint}",
           "--define", f"__gpg_sign_cmd {sign_cmd}", "--addsign", str(rpm)]
    run = subprocess.run(cmd, capture_output=True, check=False, timeout=300)
    if run.returncode != 0:
        raise SigningError(f"rpmsign failed for {rpm.name} ({run.returncode})")


def build_repo(rpms: list[Path], out: Path, channel: str, signer: Signer) -> Path:
    if channel not in CHANNELS:
        raise RepoError(f"unknown channel {channel!r}: expected one of {CHANNELS}")
    if not rpms:
        raise RepoError("no .rpm files given")
    createrepo = need("createrepo_c")
    need("rpmsign")
    root = out / "rpm" / channel
    if root.exists():
        shutil.rmtree(root)
    packages = root / "Packages"
    packages.mkdir(parents=True)
    for rpm in sorted(rpms):
        dest = packages / rpm.name
        shutil.copyfile(rpm, dest)
        sign_package(dest, signer)
    run = subprocess.run([createrepo, "--general-compress-type", "gz", str(root)],
                         capture_output=True, check=False, timeout=300)
    if run.returncode != 0:
        raise RepoError(f"createrepo_c failed ({run.returncode})")
    sign_repomd(root, signer)
    return root


def sign_repomd(root: Path, signer: Signer) -> Path:
    """Detached armoured signature of ``repodata/repomd.xml`` (what ``repo_gpgcheck=1`` fetches)."""
    repomd = root / "repodata" / "repomd.xml"
    if not repomd.is_file():
        raise RepoError("repodata/repomd.xml is missing")
    return signer.detached(repomd, root / "repodata" / "repomd.xml.asc", armor=True)


def _read_xml(path: Path) -> bytes:
    data = path.read_bytes()
    if path.name.endswith(".gz"):
        return gzip.decompress(data)
    if path.name.endswith(".xz"):
        return lzma.decompress(data)
    if path.name.endswith(".zst"):
        from compression import zstd  # Python 3.14+
        return zstd.decompress(data)
    return data


def _sha(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def verify_repo(root: Path, keyring: Path) -> None:
    """Client model of ``repo_gpgcheck=1`` and the package checksums. Raises :class:`RepoError`."""
    repomd = root / "repodata" / "repomd.xml"
    asc = root / "repodata" / "repomd.xml.asc"
    if not repomd.is_file() or not asc.is_file():
        raise RepoError("repomd.xml or repomd.xml.asc is missing")
    if not verify(keyring, asc, repomd):
        raise RepoError("repomd.xml signature is not valid for the pinned keys")
    tree = ET.fromstring(repomd.read_bytes())
    primary: Path | None = None
    for data in tree.findall(f"{REPO_NS}data"):
        location = data.find(f"{REPO_NS}location").get("href")  # type: ignore[union-attr]
        checksum = data.find(f"{REPO_NS}checksum")
        target = root / location
        if not target.is_file():
            raise RepoError(f"{location} is listed in repomd.xml but missing")
        if checksum is None or checksum.get("type") != "sha256" or _sha(target) != (checksum.text or "").strip():
            raise RepoError(f"{location} does not match repomd.xml")
        if data.get("type") == "primary":
            primary = target
    if primary is None:
        raise RepoError("repomd.xml has no primary metadata")
    for package in ET.fromstring(_read_xml(primary)).findall(f"{COMMON_NS}package"):
        location = package.find(f"{COMMON_NS}location").get("href")  # type: ignore[union-attr]
        checksum = package.find(f"{COMMON_NS}checksum")
        target = root / location
        if not target.is_file():
            raise RepoError(f"{location} is listed in primary but missing")
        if checksum is None or checksum.get("type") != "sha256" or _sha(target) != (checksum.text or "").strip():
            raise RepoError(f"{location} does not match primary")


def rpm_checksig(rpm: Path, public_key: bytes) -> bool:
    """``rpm -K`` against a throwaway rpm database that trusts only ``public_key``."""
    rpm_bin = need("rpm")
    with tempfile.TemporaryDirectory(prefix="spot-rpmdb-") as tmp:
        key = Path(tmp) / "key.asc"
        key.write_bytes(public_key)
        base = [rpm_bin, "--dbpath", str(Path(tmp) / "db")]
        subprocess.run(base + ["--initdb"], capture_output=True, check=False, timeout=60)
        subprocess.run(base + ["--import", str(key)], capture_output=True, check=False, timeout=60)
        run = subprocess.run(base + ["-K", str(rpm)], capture_output=True, text=True, check=False, timeout=60)
    return run.returncode == 0 and "NOT OK" not in run.stdout and "NOKEY" not in run.stdout
