"""Reading and verifying a signed component package.

The layout is the one ``clients/avalonia/src/Spotledger.Component.Host/README.md`` documents:
``component.json`` (the signed manifest), ``component.sig`` (standard base64 of the 64 signature
bytes), and the assemblies and assets the manifest lists. This tool reads the package the same way
the desktop host does, so a package that a shell can bundle is a package the client would install:
every refusal names a code, and the codes are the host's.
"""

from __future__ import annotations

import base64
import binascii
import hashlib
import json
import posixpath
from dataclasses import dataclass
from typing import Any, Final

from . import ed25519
from .canonical import MANIFEST_DOMAIN, signed_bytes

#: The manifest's file name.
MANIFEST_FILE: Final = "component.json"

#: The detached signature's file name.
SIGNATURE_FILE: Final = "component.sig"

#: The members a manifest has, and nothing else.
MANIFEST_MEMBERS: Final = frozenset(
    {
        "assemblies",
        "assets",
        "component_id",
        "component_version",
        "contract_range",
        "display_name",
        "entry_assembly",
        "key_id",
        "publisher_id",
        "signed_at",
    }
)

#: The members of a file entry.
FILE_MEMBERS: Final = frozenset({"path", "sha256"})

#: The members of a contract range.
RANGE_MEMBERS: Final = frozenset({"maximum_exclusive", "minimum"})

#: The refusal codes, the ones the desktop host raises.
MANIFEST_MISSING: Final = "component.package.manifest_missing"
MANIFEST_INVALID: Final = "component.manifest.invalid"
SIGNATURE_MISSING: Final = "component.package.signature_missing"
SIGNATURE_MALFORMED: Final = "component.signature.malformed"
SIGNATURE_INVALID: Final = "component.signature.invalid"
FILE_MISSING: Final = "component.package.file_missing"
PATH_INVALID: Final = "component.package.path_invalid"
HASH_MISMATCH: Final = "component.package.hash_mismatch"
UNKNOWN_KEY: Final = "component.signature.unknown_key"
PUBLISHER_MISMATCH: Final = "component.publisher.mismatch"
KEY_REVOKED: Final = "component.key.revoked"
PUBLISHER_REVOKED: Final = "component.publisher.revoked"


class Refused(Exception):
    """A package the build will not bundle, with the host's stable code and a readable message."""

    def __init__(self, code: str, message: str) -> None:
        super().__init__(f"{code}: {message}")
        self.code = code
        self.message = message


@dataclass(frozen=True)
class ManifestFile:
    """One file a manifest lists: its package-relative path and the SHA-256 of its bytes."""

    path: str
    sha256: str


@dataclass(frozen=True)
class Manifest:
    """A parsed ``component.json``: the identity the signature covers, and the files it lists."""

    component_id: str
    display_name: str
    component_version: str
    publisher_id: str
    minimum: str
    maximum_exclusive: str
    entry_assembly: str
    assemblies: tuple[ManifestFile, ...]
    assets: tuple[ManifestFile, ...]
    key_id: str
    signed_at: int

    @property
    def files(self) -> tuple[ManifestFile, ...]:
        """Every listed file, assemblies then assets."""
        return self.assemblies + self.assets

    def canonical(self) -> dict[str, Any]:
        """The object the signature covers, with the member names the Rust side writes."""
        return {
            "assemblies": [{"path": f.path, "sha256": f.sha256} for f in self.assemblies],
            "assets": [{"path": f.path, "sha256": f.sha256} for f in self.assets],
            "component_id": self.component_id,
            "component_version": self.component_version,
            "contract_range": {
                "maximum_exclusive": self.maximum_exclusive,
                "minimum": self.minimum,
            },
            "display_name": self.display_name,
            "entry_assembly": self.entry_assembly,
            "key_id": self.key_id,
            "publisher_id": self.publisher_id,
            "signed_at": self.signed_at,
        }

    def signed_bytes(self) -> bytes:
        """The exact bytes the publisher's Ed25519 signature covers."""
        return signed_bytes(MANIFEST_DOMAIN, self.canonical())

    def digest(self) -> str:
        """The SHA-256 of :meth:`signed_bytes`: the package's identity in a revocation list."""
        return hashlib.sha256(self.signed_bytes()).hexdigest()


@dataclass(frozen=True)
class Package:
    """A package as it was read: the manifest, the signature and every listed file's bytes."""

    root: str
    manifest: Manifest
    signature: bytes
    files: dict[str, bytes]

    @property
    def entry_assembly(self) -> bytes:
        """The bytes of the assembly the host loads."""
        return self.files[self.manifest.entry_assembly]


def hash_bytes(data: bytes) -> str:
    """The SHA-256 of ``data``, lower-case hex: what a manifest lists."""
    return hashlib.sha256(data).hexdigest()


def is_safe_relative_path(path: object) -> bool:
    """True when ``path`` is a package-relative path that cannot leave the package."""
    if not isinstance(path, str) or not path or len(path) > 512:
        return False
    if "\\" in path or ":" in path or path.startswith("/"):
        return False
    for segment in path.split("/"):
        if segment in ("", ".", "..") or len(segment) > 128:
            return False
    return posixpath.normpath(path) == path


def _is_hex(value: object, length: int) -> bool:
    return (
        isinstance(value, str)
        and len(value) == length
        and all(character in "0123456789abcdef" for character in value)
    )


def _is_dotted_id(value: object) -> bool:
    """The T1 contract's component id shape: a dotted lower-case id such as ``acme.rating``."""
    if not isinstance(value, str) or not value or value[0] < "a" or value[0] > "z":
        return False
    previous_separator = False
    saw_separator = False
    for character in value:
        if character in ".-":
            if previous_separator:
                return False
            previous_separator = True
            saw_separator = True
            continue
        if not (character.isascii() and (character.islower() or character.isdigit() or character == "_")):
            return False
        previous_separator = False
    return saw_separator and not previous_separator


def _is_publisher_id(value: object) -> bool:
    return (
        isinstance(value, str)
        and bool(value)
        and value[0] >= "a"
        and value[0] <= "z"
        and all(
            character.isascii() and (character.islower() or character.isdigit() or character == "_")
            for character in value
        )
    )


def _is_version(value: object) -> bool:
    if not isinstance(value, str) or not value:
        return False
    parts = value.split(".")
    return 1 <= len(parts) <= 4 and all(part.isdigit() and str(int(part)) == part for part in parts)


def _without_build_metadata(version: str) -> str:
    """A semver without its ``+build`` part: the contract range compares the released version."""
    return version.split("+", 1)[0]


def _contract_range(raw: object, problems: list[str]) -> tuple[str, str]:
    if not isinstance(raw, dict):
        problems.append("manifest.contract_range: 'contract_range' is an object")
        return "", ""
    if set(raw) - RANGE_MEMBERS:
        problems.append("manifest.contract_range: no such member as " + ", ".join(sorted(set(raw) - RANGE_MEMBERS)))
    minimum = raw.get("minimum")
    maximum = raw.get("maximum_exclusive")
    if not _is_version(minimum) or not _is_version(maximum):
        problems.append("manifest.contract_range: minimum and maximum_exclusive are versions")
        return "", ""
    return _without_build_metadata(minimum), _without_build_metadata(maximum)


def _files(raw: object, name: str, problems: list[str]) -> tuple[ManifestFile, ...]:
    if raw is None:
        return ()
    if not isinstance(raw, list):
        problems.append(f"manifest.{name}: '{name}' is an array of {{path, sha256}} objects")
        return ()
    files: list[ManifestFile] = []
    for item in raw:
        if not isinstance(item, dict):
            problems.append(f"manifest.{name}: every entry is an object")
            continue
        if set(item) - FILE_MEMBERS:
            problems.append(f"manifest.{name}: no such member as " + ", ".join(sorted(set(item) - FILE_MEMBERS)))
        path, digest = item.get("path"), item.get("sha256")
        if not is_safe_relative_path(path):
            problems.append(f"manifest.{name}: '{path}' is a safe package-relative path")
            continue
        if not _is_hex(digest, 64):
            problems.append(f"manifest.{name}: the sha256 of '{path}' is 64 lower-case hex characters")
            continue
        files.append(ManifestFile(path, digest))
    paths = [f.path for f in files]
    if len(set(paths)) != len(paths):
        problems.append(f"manifest.{name}: a path is listed twice")
    return tuple(sorted(files, key=lambda f: f.path))


def parse_manifest(text: str) -> Manifest:
    """Parses a ``component.json``, refusing the whole document when any rule is broken."""
    try:
        raw = json.loads(text)
    except json.JSONDecodeError as error:
        raise Refused(MANIFEST_INVALID, f"manifest is not JSON: {error}") from error
    if not isinstance(raw, dict):
        raise Refused(MANIFEST_INVALID, "manifest is a JSON object")

    problems: list[str] = []
    unknown = set(raw) - MANIFEST_MEMBERS
    if unknown:
        problems.append("manifest: no such member as " + ", ".join(sorted(unknown)))

    component_id = raw.get("component_id")
    if not _is_dotted_id(component_id):
        problems.append(f"manifest.component_id: '{component_id}' is a dotted lowercase id such as 'acme.rating'")
    publisher_id = raw.get("publisher_id")
    if not _is_publisher_id(publisher_id):
        problems.append(f"manifest.publisher_id: '{publisher_id}' is a lowercase id such as 'acme'")
    display_name = raw.get("display_name")
    if not isinstance(display_name, str) or not 1 <= len(display_name) <= 64:
        problems.append("manifest.display_name: DisplayName is 1 to 64 characters")
    version = raw.get("component_version")
    if not _is_version(version):
        problems.append(f"manifest.component_version: '{version}' is a version such as 1.0.0")
    key_id = raw.get("key_id")
    if not _is_hex(key_id, 32):
        problems.append("manifest.key_id: key_id is 32 lower-case hex characters")
    signed_at = raw.get("signed_at")
    if not isinstance(signed_at, int) or isinstance(signed_at, bool) or signed_at < 0:
        problems.append("manifest.signed_at: signed_at is a non-negative integer (UTC seconds)")
    entry = raw.get("entry_assembly")
    if not is_safe_relative_path(entry):
        problems.append(f"manifest.entry_assembly: '{entry}' is a safe package-relative path")
    minimum, maximum = _contract_range(raw.get("contract_range"), problems)
    assemblies = _files(raw.get("assemblies"), "assemblies", problems)
    assets = _files(raw.get("assets"), "assets", problems)
    if not any(f.path == entry for f in assemblies):
        problems.append("manifest.entry_assembly: the entry assembly is one of the listed assemblies")

    if problems:
        raise Refused(MANIFEST_INVALID, " ".join(problems))

    return Manifest(
        component_id=str(component_id),
        display_name=str(display_name),
        component_version=str(version),
        publisher_id=str(publisher_id),
        minimum=minimum,
        maximum_exclusive=maximum,
        entry_assembly=str(entry),
        assemblies=assemblies,
        assets=assets,
        key_id=str(key_id),
        signed_at=int(signed_at),
    )


def read_package(directory: str, *, read_bytes=None, exists=None) -> Package:
    """Reads a package directory: the manifest, the signature and every listed file's bytes."""
    import os

    read_bytes = read_bytes or (lambda path: open(path, "rb").read())
    exists = exists or os.path.isfile

    manifest_path = os.path.join(directory, MANIFEST_FILE)
    if not exists(manifest_path):
        raise Refused(MANIFEST_MISSING, f"'{MANIFEST_FILE}' is missing from '{directory}'")
    try:
        text = read_bytes(manifest_path).decode("utf-8")
    except UnicodeDecodeError as error:
        raise Refused(MANIFEST_INVALID, f"'component.json' is UTF-8: {error}") from error
    manifest = parse_manifest(text)

    signature_path = os.path.join(directory, SIGNATURE_FILE)
    if not exists(signature_path):
        raise Refused(SIGNATURE_MISSING, f"'{SIGNATURE_FILE}' is missing: an unsigned package is refused")
    raw_signature = read_bytes(signature_path).strip()
    try:
        signature = base64.b64decode(raw_signature, validate=True)
    except (binascii.Error, ValueError) as error:
        raise Refused(SIGNATURE_MALFORMED, f"'{SIGNATURE_FILE}' is not standard base64: {error}") from error
    if len(signature) != ed25519.SIGNATURE_BYTES:
        raise Refused(SIGNATURE_MALFORMED, f"'{SIGNATURE_FILE}' is not standard base64 of 64 bytes")

    files: dict[str, bytes] = {}
    for listed in manifest.files:
        path = os.path.join(directory, listed.path.replace("/", os.sep))
        if not exists(path):
            raise Refused(FILE_MISSING, f"the manifest lists '{listed.path}' but the package does not contain it")
        data = read_bytes(path)
        actual = hash_bytes(data)
        if actual != listed.sha256:
            raise Refused(
                HASH_MISMATCH,
                f"'{listed.path}' hashes to {actual}, but the manifest lists {listed.sha256}: "
                "the package was modified after it was signed",
            )
        files[listed.path] = data

    return Package(root=directory, manifest=manifest, signature=signature, files=files)


def verify(package: Package, keys: dict[str, dict[str, str]], *, revoked_keys=(), revoked_publishers=()) -> None:
    """Verifies a package's signature against the store's publisher keys.

    ``keys`` maps a key id to ``{"public_key": <base64>, "publisher_id": ...}``, which is what the
    store registry supplies for a build. A revoked key or publisher is refused here; the desktop host
    also checks a signed revocation list with effective dates, which a build has no clock for.
    """
    manifest = package.manifest
    if manifest.key_id in revoked_keys:
        raise Refused(KEY_REVOKED, f"the signing key {manifest.key_id} is revoked")
    if manifest.publisher_id in revoked_publishers:
        raise Refused(PUBLISHER_REVOKED, f"the publisher '{manifest.publisher_id}' is revoked")

    entry = keys.get(manifest.key_id)
    if entry is None:
        raise Refused(UNKNOWN_KEY, f"the signing key {manifest.key_id} is not named by the store's trust material")
    try:
        public_key = base64.b64decode(entry["public_key"], validate=True)
    except (binascii.Error, ValueError, KeyError) as error:
        raise Refused(UNKNOWN_KEY, f"the key {manifest.key_id} has no readable public key: {error}") from error
    if len(public_key) != ed25519.PUBLIC_KEY_BYTES or ed25519.key_id(public_key) != manifest.key_id:
        raise Refused(
            UNKNOWN_KEY,
            f"the key listed under {manifest.key_id} is not that key id's 32-byte public key",
        )
    # A trusted key names its publisher. A key with no publisher would verify a package that claims
    # any publisher, so it is refused rather than waved through.
    if entry.get("publisher_id") != manifest.publisher_id:
        raise Refused(
            PUBLISHER_MISMATCH,
            f"the manifest says it was published by '{manifest.publisher_id}', "
            f"but key {manifest.key_id} belongs to '{entry.get('publisher_id')}'",
        )
    if not ed25519.verify(public_key, manifest.signed_bytes(), package.signature):
        raise Refused(
            SIGNATURE_INVALID,
            f"the signature does not verify over the canonical manifest of '{manifest.component_id}'",
        )
