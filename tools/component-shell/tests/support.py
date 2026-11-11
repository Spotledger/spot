"""Shared fixtures for the component-shell tests: keys, signed packages and temporary folders.

The signing here is the tool's own Ed25519 implementation signed with a fixed seed, and the vectors
come from the checked-in file the Rust crate generated — so the tests prove the tool against real
signed bytes, not against itself.
"""

from __future__ import annotations

import base64
import hashlib
import json
import os
import sys
import tempfile
import unittest
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Iterable

# The tool is a script package under tools/component-shell, not an installed distribution.
_TOOL_DIRECTORY = Path(__file__).resolve().parents[1]
if str(_TOOL_DIRECTORY) not in sys.path:
    sys.path.insert(0, str(_TOOL_DIRECTORY))

from component_shell import canonical, ed25519, manifest, trust  # noqa: E402
from component_shell.package import Package, read_package, verify  # noqa: E402

REPOSITORY_ROOT = _TOOL_DIRECTORY.parents[1]

#: The Rust-generated cross-language vectors, next to the C# host's tests.
VECTORS = (
    REPOSITORY_ROOT
    / "clients"
    / "avalonia"
    / "tests"
    / "Spotledger.Component.Host.Tests"
    / "Vectors"
    / "component-signing-vectors.json"
)

#: The content-addressed set manifest the C# host's tests read too.
SET_SAMPLE = VECTORS.parent / "component-set-eadd8795b07d255e.json"

#: A fixed publisher seed: public test material, no secret.
PUBLISHER_SEED = bytes.fromhex("404142434445464748494a4b4c4d4e4f505152535455565758595a5b5c5d5e5f")

#: A second key, for the wrong-key cases.
OUTSIDER_SEED = bytes.fromhex("c0c1c2c3c4c5c6c7c8c9cacbcccdcecfd0d1d2d3d4d5d6d7d8d9dadbdcdddedf")


def load_vectors() -> dict[str, Any]:
    """The vector file, as JSON."""
    with VECTORS.open(encoding="utf-8") as handle:
        return json.load(handle)


def trust_for(seed: bytes = PUBLISHER_SEED, publisher: str = "acme") -> dict[str, dict[str, str]]:
    """The store material for one publisher key."""
    public = ed25519.public_key(seed)
    return {ed25519.key_id(public): {"public_key": base64.b64encode(public).decode("ascii"), "publisher_id": publisher}}


def hash_bytes(data: bytes) -> str:
    """The SHA-256 of ``data``, lower-case hex."""
    return hashlib.sha256(data).hexdigest()


class TempDirectory:
    """A temporary folder that removes itself."""

    def __init__(self) -> None:
        self.path = Path(tempfile.mkdtemp(prefix="spot-component-shell-"))

    def __enter__(self) -> Path:
        return self.path

    def __exit__(self, *_exc: object) -> None:
        import shutil

        shutil.rmtree(self.path, ignore_errors=True)


def build_package(
    root: Path,
    *,
    component_id: str = "acme.rating",
    version: str = "1.0.0",
    publisher_id: str = "acme",
    seed: bytes = PUBLISHER_SEED,
    names_key_id: str | None = None,
    assembly: bytes = b"MZ fake assembly bytes for a test",
    assembly_path: str = "assemblies/Acme.RatingComponent.dll",
    assets: dict[str, bytes] | None = None,
    signed_at: int = 1_760_000_000,
    signature: bytes | None = None,
    write_signature: bool = True,
    extra_members: dict[str, Any] | None = None,
    minimum: str = "1.0",
    maximum_exclusive: str = "2.0",
) -> Package | None:
    """Writes a signed package into ``root`` and reads it back the way the tool would.

    A fixture that is deliberately broken (unsigned, tampered, unsafe path) does not read back, and
    that is the point: the return value is None there and the test reads the package itself.
    """
    assets = assets or {}
    root.mkdir(parents=True, exist_ok=True)
    from component_shell.package import is_safe_relative_path

    # A path that leaves the package is a case under test: the manifest is written anyway and the file
    # is not, because writing it would put bytes outside the fixture's own directory.
    if is_safe_relative_path(assembly_path):
        assembly_file = root / assembly_path
        assembly_file.parent.mkdir(parents=True, exist_ok=True)
        assembly_file.write_bytes(assembly)
    for path, data in assets.items():
        asset_file = root / path
        asset_file.parent.mkdir(parents=True, exist_ok=True)
        asset_file.write_bytes(data)

    body: dict[str, Any] = {
        "assemblies": [{"path": assembly_path, "sha256": hash_bytes(assembly)}],
        "assets": [{"path": path, "sha256": hash_bytes(data)} for path, data in sorted(assets.items())],
        "component_id": component_id,
        "component_version": version,
        "contract_range": {"maximum_exclusive": maximum_exclusive, "minimum": minimum},
        "display_name": "Test component",
        "entry_assembly": assembly_path,
        "key_id": names_key_id or ed25519.key_id(ed25519.public_key(seed)),
        "publisher_id": publisher_id,
        "signed_at": signed_at,
    }
    body.update(extra_members or {})
    text = canonical.encode(body)
    (root / "component.json").write_text(text, encoding="utf-8")
    if write_signature:
        signature_bytes = (
            signature
            if signature is not None
            else ed25519.sign(seed, canonical.signed_bytes(canonical.MANIFEST_DOMAIN, body))
        )
        (root / "component.sig").write_text(base64.b64encode(signature_bytes).decode("ascii"), encoding="utf-8")

    from component_shell.package import Refused

    try:
        return read_package(str(root))
    except Refused:
        return None


def write_set_file(
    path: Path,
    *,
    name: str = "test-shell",
    packages: Iterable[Path] = (),
    keys: dict[str, dict[str, str]] | None = None,
    revoked_keys: Iterable[str] = (),
    revoked_publishers: Iterable[str] = (),
    trust_table: bool = True,
) -> Path:
    """Writes a set file the CLI can read.

    ``trust_table`` is False for the default mode: a set file there names the shell and the packages
    and no trust material at all, which is the point of a recorded security finding.
    """
    keys = keys if keys is not None else trust_for()
    lines = ["[shell]", f'name = "{name}"', ""]
    if trust_table:
        lines += ["[trust]"]
        lines.append("revoked_keys = [" + ", ".join(f'"{key}"' for key in revoked_keys) + "]")
        lines.append("revoked_publishers = [" + ", ".join(f'"{value}"' for value in revoked_publishers) + "]")
        lines.append("")
        lines.append("[trust.keys]")
        for key_id, value in keys.items():
            lines.append(
                f'"{key_id}" = {{ public_key = "{value["public_key"]}", '
                f'publisher_id = "{value.get("publisher_id", "acme")}" }}'
            )
    for package in packages:
        lines += ["", "[[component]]", f'package = "{os.path.relpath(package, path.parent)}"']
    path.write_text("\n".join(lines) + "\n", encoding="utf-8")
    return path


#: The instant every trust test judges windows and effective dates at.
AS_OF = 1_760_000_000


def generated_seed() -> bytes:
    """A fresh 32-byte Ed25519 seed, made in the test: no key material is committed to the repository."""
    return os.urandom(32)


def encoded(value: Any, *, compact: bool = True) -> str:
    """``value`` as the JSON a document file holds (not the canonical signed form)."""
    if compact:
        return json.dumps(value, sort_keys=True, separators=(",", ":"))
    return json.dumps(value, indent=2, sort_keys=True)


def _body_of_certificate(certificate: dict[str, Any]) -> dict[str, Any]:
    """The seven members a certificate's issuing root signs."""
    return {
        "display_name": certificate["display_name"],
        "issued_by": certificate["issued_by"],
        "publisher_id": certificate["publisher_id"],
        "public_key": certificate["public_key"],
        "serial": certificate["serial"],
        "valid_from": certificate["valid_from"],
        "valid_to": certificate["valid_to"],
    }


def certificate_document(
    *,
    publisher_seed: bytes,
    root_seed: bytes,
    publisher_id: str = "acme",
    display_name: str = "Acme Components",
    valid_from: int = 1_750_000_000,
    valid_to: int = 1_800_000_000,
    serial: int = 1,
) -> dict[str, Any]:
    """A publisher certificate signed by ``root_seed``, in the store's file form."""
    public = ed25519.public_key(publisher_seed)
    body = {
        "display_name": display_name,
        "issued_by": ed25519.key_id(ed25519.public_key(root_seed)),
        "publisher_id": publisher_id,
        "public_key": base64.b64encode(public).decode("ascii"),
        "serial": serial,
        "valid_from": valid_from,
        "valid_to": valid_to,
    }
    signature = ed25519.sign(root_seed, canonical.signed_bytes(canonical.CERTIFICATE_DOMAIN, body))
    return {
        **body,
        "format": "spot-publisher-certificate",
        "version": 1,
        "signature": base64.b64encode(signature).decode("ascii"),
    }


def root_entry(seed: bytes, *, valid_from: int = 0, valid_to: int | None = None) -> dict[str, Any]:
    """One ``roots`` entry of an anchor set."""
    public = ed25519.public_key(seed)
    return {
        "key_id": ed25519.key_id(public),
        "public_key": base64.b64encode(public).decode("ascii"),
        "valid_from": valid_from,
        "valid_to": valid_to,
    }


def anchor_set_document(
    *,
    root_seed: bytes,
    certificates: Iterable[dict[str, Any]],
    roots: Iterable[dict[str, Any]] | None = None,
    generation: int = 1,
    issued_at: int = AS_OF,
    sign_with: bytes | None = None,
) -> str:
    """A root-signed anchor set, as a JSON file's text."""
    roots = list(roots) if roots is not None else [root_entry(root_seed)]
    certificates = list(certificates)
    body = {
        "certificates": [_body_of_certificate(certificate) for certificate in certificates],
        "generation": generation,
        "issued_at": issued_at,
        "roots": roots,
    }
    signature = ed25519.sign(sign_with or root_seed, canonical.signed_bytes("SPOT-COMPONENT-ANCHOR-SET-V1\n", body))
    document = {
        **body,
        "format": "spot-component-anchor-set",
        "version": 1,
        "certificates": certificates,
        "signature": base64.b64encode(signature).decode("ascii"),
    }
    return encoded(document)


def revoked_key(key_seed: bytes, *, effective_from: int = AS_OF, compromised: bool = False) -> dict[str, Any]:
    """One ``keys`` entry of a revocation list."""
    return {
        "key_id": ed25519.key_id(ed25519.public_key(key_seed)),
        "effective_from": effective_from,
        "compromised": compromised,
        "reason": "test revocation",
    }


def revoked_publisher(publisher_id: str = "acme", *, effective_from: int = AS_OF) -> dict[str, Any]:
    """One ``publishers`` entry of a revocation list."""
    return {"publisher_id": publisher_id, "effective_from": effective_from, "reason": "test revocation"}


def revoked_package(digest: str) -> dict[str, Any]:
    """One ``bundles`` entry of a revocation list."""
    return {"digest": digest, "reason": "test revocation"}


def revocation_document(
    *,
    root_seed: bytes,
    keys: Iterable[dict[str, Any]] = (),
    publishers: Iterable[dict[str, Any]] = (),
    packages: Iterable[dict[str, Any]] = (),
    generation: int = 1,
    issued_at: int = AS_OF,
    sign_with: bytes | None = None,
) -> str:
    """A root-signed revocation list, as a JSON file's text."""
    body = {
        "bundles": sorted(packages, key=lambda entry: entry["digest"]),
        "generation": generation,
        "issued_at": issued_at,
        "issued_by": ed25519.key_id(ed25519.public_key(sign_with or root_seed)),
        "keys": sorted(keys, key=lambda entry: entry["key_id"]),
        "publishers": sorted(publishers, key=lambda entry: entry["publisher_id"]),
    }
    signature = ed25519.sign(
        sign_with or root_seed, canonical.signed_bytes(canonical.REVOCATION_DOMAIN, body)
    )
    document = {**body, "format": "spot-revocation-list", "version": 1, "signature": base64.b64encode(signature).decode("ascii")}
    return encoded(document)


def root_key_document(seed: bytes) -> str:
    """The ``--root-key`` file: one root's public key and key id."""
    public = ed25519.public_key(seed)
    return encoded(
        {
            "format": "spot-trust-root-key",
            "version": 1,
            "key_id": ed25519.key_id(public),
            "public_key": base64.b64encode(public).decode("ascii"),
        }
    )


@dataclass(frozen=True)
class TrustFiles:
    """The three trust documents a build was given, and the arguments that name them."""

    root_key: Path
    anchor_set: Path
    revocations: Path
    as_of: int
    min_anchors_generation: int = 1
    min_revocation_generation: int = 1

    def arguments(self) -> list[str]:
        """The command-line arguments for the default mode."""
        return [
            "--root-key",
            str(self.root_key),
            "--anchor-set",
            str(self.anchor_set),
            "--revocations",
            str(self.revocations),
            "--as-of",
            str(self.as_of),
            "--min-anchors-generation",
            str(self.min_anchors_generation),
            "--min-revocation-generation",
            str(self.min_revocation_generation),
        ]

    def material(self) -> Any:
        """The verified material, for a test that wants its digest."""
        return trust.load_trust_material(
            anchor_set=self.anchor_set.read_bytes(),
            revocations=self.revocations.read_bytes(),
            root_key=self.root_key.read_bytes(),
            as_of=self.as_of,
            min_anchors_generation=self.min_anchors_generation,
            min_revocation_generation=self.min_revocation_generation,
        )


def write_trust(
    directory: Path,
    *,
    publisher_seed: bytes = PUBLISHER_SEED,
    root_seed: bytes | None = None,
    certificates: Iterable[dict[str, Any]] | None = None,
    revocations: str | None = None,
    roots: Iterable[dict[str, Any]] | None = None,
    as_of: int = AS_OF,
    published_at: int = AS_OF,
    anchors_generation: int = 1,
    revocation_generation: int = 1,
) -> TrustFiles:
    """Writes a root key, an anchor set and a revocation list into ``directory``.

    The root is generated here unless a test passes one, so the fixtures hold no committed key: a
    test that wants a second root calls ``generated_seed()`` itself.
    The pins default to the generation each document was written with, so a fixture that raises a
    generation is never silently built by a pin that would still accept the older one.
    """
    directory.mkdir(parents=True, exist_ok=True)
    root_seed = root_seed or generated_seed()
    root_entries = list(roots) if roots is not None else [root_entry(root_seed, valid_from=0, valid_to=None)]
    if certificates is None:
        certificates = [certificate_document(publisher_seed=publisher_seed, root_seed=root_seed)]
    certificates = list(certificates)

    root_key = directory / "root-key.json"
    anchor_set = directory / "anchor-set.json"
    revocations_file = directory / "revocations.json"
    root_key.write_text(root_key_document(root_seed), encoding="utf-8")
    anchor_set.write_text(
        anchor_set_document(
            root_seed=root_seed,
            certificates=certificates,
            roots=root_entries,
            issued_at=published_at,
            generation=anchors_generation,
        ),
        encoding="utf-8",
    )
    revocations_file.write_text(
        revocations if revocations is not None else revocation_document(root_seed=root_seed, issued_at=published_at, generation=revocation_generation),
        encoding="utf-8",
    )
    return TrustFiles(
        root_key=root_key,
        anchor_set=anchor_set,
        revocations=revocations_file,
        as_of=as_of,
        min_anchors_generation=anchors_generation,
        min_revocation_generation=revocation_generation,
    )


def write_reviewed_list(path: Path, digests: Iterable[str]) -> Path:
    """Writes a reviewed-sets file: a header comment and one digest per line."""
    lines = ["# Set files a reviewer read: the SHA-256 of each file's bytes, one per line.", ""]
    lines += list(digests)
    path.write_text("\n".join(lines) + "\n", encoding="utf-8")
    return path


def set_digest(path: Path) -> str:
    """The SHA-256 of a set file's bytes, which is what a reviewed-sets file records."""
    return hashlib.sha256(path.read_bytes()).hexdigest()



class ComponentShellTestCase(unittest.TestCase):
    """A base class with the two helpers every suite wants."""

    def assert_refused(self, code: str, call, *args, **kwargs) -> None:  # noqa: ANN001 - unittest style
        """Asserts that ``call`` raises a refusal carrying ``code``."""
        from component_shell.package import Refused

        with self.assertRaises(Refused) as caught:
            call(*args, **kwargs)
        self.assertEqual(code, caught.exception.code, str(caught.exception))

    def manifest_sample(self) -> manifest.SetManifest:
        """The committed sample, parsed."""
        return manifest.parse(SET_SAMPLE.read_text(encoding="utf-8"))


def verify_package(package: Package, keys: dict[str, dict[str, str]] | None = None) -> None:
    """Verifies a package against the fixed publisher key."""
    verify(package, keys if keys is not None else trust_for())
