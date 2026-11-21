"""The store registry's signed trust material, and the check a package passes against it.

Before a recorded security finding a build trusted ``[trust.keys]`` in the tenant's set file: whoever could edit that file
could name their own key and bundle a package the store would refuse. The build now reads trust only
from material the store signed, and verifies it against a root key the operator supplies through a
separate file:

````
--root-key root-key.json          one root's public key, trusted out of band
--anchor-set anchor-set.json      root-signed: the trusted roots and the publisher certificates
--revocations revocations.json    root-signed by a root in that set: keys, publishers and packages
--as-of 1760000000                the instant every window and effective date is judged at
````

The anchor set is the one document this tool defines itself, because the store registry never had a
signed list of *which* roots are current (the desktop host is given its trust roots by the registry).
It is signed by the root named in ``--root-key`` with the same encoding the rest of the signing scheme
uses — a domain prefix and canonical JSON — so a store-side issuer written against
``crates/framework/spot-bundle-signing/`` produces bytes this tool accepts:

````json
{
  "format": "spot-component-anchor-set",
  "version": 1,
  "generation": 3,
  "issued_at": 1759000000,
  "roots": [
    {"key_id": "<32 hex>", "public_key": "<base64 32 bytes>",
     "valid_from": 1750000000, "valid_to": 1790000000}
  ],
  "certificates": [
    {"format": "spot-publisher-certificate", "version": 1,
     "publisher_id": "acme", "display_name": "Acme", "public_key": "<base64>",
     "valid_from": 1750000000, "valid_to": 1790000000,
     "issued_by": "<root key id>", "serial": 1, "signature": "<base64 64 bytes>"}
  ],
  "signature": "<base64 64 bytes, by the root named in --root-key>"
}
````

The signed bytes are ``SPOT-COMPONENT-ANCHOR-SET-V1\\n`` followed by the canonical JSON of
``{certificates, generation, issued_at, roots}`` — every certificate's identity fields, so a build
host cannot swap in a key the store did not certify without breaking the set's signature.

The revocation list is the store's existing document (``spot-revocation-list``,
``SPOT-REVOCATION-LIST-V1``), unchanged: the same bytes the desktop host parses, signed by a root the
anchor set names. Its entries carry ``effective_from``; a build judges them at ``--as-of`` and never
reads a clock of its own. This is the crate's model, where a signature made before a revocation stays
valid; the desktop host at install time is stricter and refuses anything the list it holds names.

What a package must satisfy, in this order (the codes are the host's where the host has one):

1. the manifest's key is named by a certificate in the verified anchor set
   (``component.signature.unknown_key``);
2. that certificate names the manifest's publisher (``component.publisher.mismatch``);
3. the root that issued the certificate is trusted at ``--as-of``
   (``component.trust.root_outside_window``);
4. the certificate's window covers ``--as-of`` (``component.certificate.not_yet_valid``,
   ``component.certificate.expired``);
5. the publisher, the signing key and the package digest are not revoked as of ``--as-of``, and a
   compromised key is refused whenever it signed (``component.publisher.revoked``,
   ``component.key.revoked``, ``component.key.compromised``, ``component.package.revoked``);
6. the signature verifies over the canonical manifest bytes with the certificate's key
   (``component.signature.invalid``).

Nothing here reads a private key: every document holds public material and signatures. No error message
carries key material beyond the key ids the documents already publish.

What that list does not cover is freshness. A genuine, older, correctly signed pair of documents
satisfies every rule above, so a build host that controls the files can replay a revocation list from
before a package was revoked. ``require_generation`` is the answer: each document's ``generation`` is
compared with a minimum the caller passed **on the command line**, after that document's own signature
verified — so an edited number fails as a bad signature, and a missing pin is an error rather than
"accept anything". ``--reviewed-set`` mode trusts a reviewed digest and no signed list, so it neither
needs nor honours the pins.
"""

from __future__ import annotations

import base64
import binascii
import hashlib
import json
import os
from dataclasses import dataclass
from datetime import datetime
from typing import Any, Final, Iterable

from . import ed25519
from .canonical import CERTIFICATE_DOMAIN, REVOCATION_DOMAIN, signed_bytes
from .package import Package, Refused

#: The domain prefix of an anchor set's signed bytes.
ANCHOR_SET_DOMAIN: Final = "SPOT-COMPONENT-ANCHOR-SET-V1\n"

#: The domain prefix of the build's trust-material digest.
TRUST_MATERIAL_DOMAIN: Final = "SPOT-COMPONENT-TRUST-MATERIAL-V1\n"

#: The ``format`` member of each document.
ANCHOR_SET_FORMAT: Final = "spot-component-anchor-set"
CERTIFICATE_FORMAT: Final = "spot-publisher-certificate"
REVOCATION_FORMAT: Final = "spot-revocation-list"
ROOT_KEY_FORMAT: Final = "spot-trust-root-key"

#: The only document version this tool reads.
FORMAT_VERSION: Final = 1

#: The members of each document, and nothing else.
ANCHOR_SET_MEMBERS: Final = frozenset(
    {"certificates", "format", "generation", "issued_at", "roots", "signature", "version"}
)
ROOT_MEMBERS: Final = frozenset({"key_id", "public_key", "valid_from", "valid_to"})
CERTIFICATE_MEMBERS: Final = frozenset(
    {
        "display_name",
        "format",
        "issued_by",
        "public_key",
        "publisher_id",
        "serial",
        "signature",
        "valid_from",
        "valid_to",
        "version",
    }
)
REVOCATION_MEMBERS: Final = frozenset(
    {
        "bundles",
        "format",
        "generation",
        "issued_at",
        "issued_by",
        "keys",
        "publishers",
        "signature",
        "version",
    }
)
REVOKED_KEY_MEMBERS: Final = frozenset({"compromised", "effective_from", "key_id", "reason"})
REVOKED_PUBLISHER_MEMBERS: Final = frozenset({"effective_from", "publisher_id", "reason"})
REVOKED_PACKAGE_MEMBERS: Final = frozenset({"digest", "reason"})
ROOT_KEY_MEMBERS: Final = frozenset({"format", "key_id", "public_key", "version"})

#: The refusal codes. The ``component.*`` ones are the desktop host's; the ``component.trust.*`` ones
#: are this tool's own, because the host is handed its trust roots and never parses these documents.
ANCHOR_SET_MALFORMED: Final = "component.trust.anchor_set_malformed"
ANCHOR_SET_SIGNATURE_INVALID: Final = "component.trust.anchor_set_signature_invalid"
ROOT_KEY_MALFORMED: Final = "component.trust.root_key_malformed"
REVOCATION_MALFORMED: Final = "component.trust.revocation_malformed"
REVOCATION_SIGNATURE_INVALID: Final = "component.revocation.signature_invalid"
UNKNOWN_TRUST_ROOT: Final = "component.trust.unknown_root"
ROOT_OUTSIDE_WINDOW: Final = "component.trust.root_outside_window"
UNKNOWN_ANCHOR: Final = "component.certificate.unknown_root"
CERTIFICATE_SIGNATURE_INVALID: Final = "component.certificate.signature_invalid"
CERTIFICATE_EXPIRED: Final = "component.certificate.expired"
CERTIFICATE_NOT_YET_VALID: Final = "component.certificate.not_yet_valid"
UNKNOWN_KEY: Final = "component.signature.unknown_key"
PUBLISHER_MISMATCH: Final = "component.publisher.mismatch"
PUBLISHER_REVOKED: Final = "component.publisher.revoked"
KEY_REVOKED: Final = "component.key.revoked"
KEY_COMPROMISED: Final = "component.key.compromised"
PACKAGE_REVOKED: Final = "component.package.revoked"
SIGNATURE_INVALID: Final = "component.signature.invalid"
ANCHORS_STALE: Final = "component.anchors.stale"
REVOCATION_STALE: Final = "component.revocation.stale"
REVIEWED_SET_NOT_RECORDED: Final = "component.trust.reviewed_set_not_recorded"
REVIEWED_LIST_MALFORMED: Final = "component.trust.reviewed_list_malformed"
SET_CARRIES_TRUST: Final = "component.trust.set_carries_keys"

#: The file name prefix and suffix of a content-addressed trust record.
RECORD_PREFIX: Final = "component-trust-"
RECORD_SUFFIX: Final = ".json"

#: The trust record's schema version.
RECORD_SCHEMA: Final = 1

#: A readable bound on any free-text ``reason`` the store puts in a revocation list.
REASON_LIMIT: Final = 200


def _malformed(code: str, detail: str) -> Refused:
    return Refused(code, detail)


def _object(text: bytes, code: str, document: str) -> dict[str, Any]:
    """The top-level JSON object of a document, or a refusal."""
    try:
        raw = json.loads(text.decode("utf-8"))
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        raise _malformed(code, f"the {document} is not UTF-8 JSON: {error}") from error
    if not isinstance(raw, dict):
        raise _malformed(code, f"the {document} is a JSON object")
    return raw


def _closed(raw: dict[str, Any], members: frozenset[str], code: str, what: str) -> None:
    """Refuses a document with a member nobody defined."""
    unknown = set(raw) - members
    if unknown:
        raise _malformed(code, f"{what}: no such member as " + ", ".join(sorted(unknown)))


def _format(raw: dict[str, Any], expected: str, code: str, what: str) -> None:
    """Refuses a document of another format or version."""
    if raw.get("format") != expected:
        raise _malformed(code, f"{what}: the format is '{expected}'")
    version = raw.get("version")
    if version != FORMAT_VERSION or isinstance(version, bool):
        raise _malformed(code, f"{what}: version {FORMAT_VERSION} is the only one this tool reads")


def _integer(value: object, code: str, what: str, *, minimum: int = 0) -> int:
    """An integer at or above ``minimum``; a bool is not an integer here."""
    if not isinstance(value, int) or isinstance(value, bool) or value < minimum:
        raise _malformed(code, f"{what} is an integer of at least {minimum}")
    return value


def _optional_integer(value: object, code: str, what: str) -> int | None:
    """An integer, or ``None`` where the document means 'no end'."""
    if value is None:
        return None
    return _integer(value, code, what)


def _hex(value: object, length: int, code: str, what: str) -> str:
    """Lower-case hex of exactly ``length`` characters."""
    if (
        not isinstance(value, str)
        or len(value) != length
        or any(character not in "0123456789abcdef" for character in value)
    ):
        raise _malformed(code, f"{what} is {length} lower-case hex characters")
    return value


def _text(value: object, code: str, what: str, *, limit: int = 200) -> str:
    """A non-empty string of at most ``limit`` characters."""
    if not isinstance(value, str) or not value or len(value) > limit:
        raise _malformed(code, f"{what} is 1 to {limit} characters")
    return value


def _base64(value: object, length: int, code: str, what: str) -> bytes:
    """Standard base64 of exactly ``length`` bytes."""
    if not isinstance(value, str):
        raise _malformed(code, f"{what} is standard base64")
    try:
        raw = base64.b64decode(value, validate=True)
    except (binascii.Error, ValueError) as error:
        raise _malformed(code, f"{what} is standard base64: {error}") from error
    if len(raw) != length:
        raise _malformed(code, f"{what} is standard base64 of {length} bytes")
    return raw


def _signature(value: object, code: str, what: str) -> bytes:
    return _base64(value, ed25519.SIGNATURE_BYTES, code, what)


def _public_key(value: object, code: str, what: str) -> bytes:
    return _base64(value, ed25519.PUBLIC_KEY_BYTES, code, what)


def _key_id_matches(public_key: bytes, key_id: str, code: str, what: str) -> None:
    """A key id is the key's own; a document that pairs them wrongly is refused."""
    if ed25519.key_id(public_key) != key_id:
        raise _malformed(code, f"{what}: the key id is not that public key's")


@dataclass(frozen=True)
class RootKey:
    """The root public key the operator supplied out of band, and its key id."""

    key_id: str
    public_key: bytes


@dataclass(frozen=True)
class Root:
    """One trusted root and the window it is trusted in; ``valid_to`` of ``None`` means no end."""

    key_id: str
    public_key: bytes
    valid_from: int
    valid_to: int | None

    def valid_at(self, instant: int) -> bool:
        """True when the root is trusted at ``instant`` (UTC seconds)."""
        return self.valid_from <= instant and (self.valid_to is None or instant < self.valid_to)


@dataclass(frozen=True)
class Certificate:
    """A publisher certificate: a publisher's key, named and dated, issued by a store root."""

    publisher_id: str
    display_name: str
    public_key: bytes
    valid_from: int
    valid_to: int
    issued_by: str
    serial: int
    signature: bytes

    @property
    def key_id(self) -> str:
        """The key id of the publisher's key."""
        return ed25519.key_id(self.public_key)

    def canonical(self) -> dict[str, Any]:
        """The body the issuing root signed, as ``certificate.rs`` writes it."""
        return {
            "display_name": self.display_name,
            "issued_by": self.issued_by,
            "publisher_id": self.publisher_id,
            "public_key": base64.b64encode(self.public_key).decode("ascii"),
            "serial": self.serial,
            "valid_from": self.valid_from,
            "valid_to": self.valid_to,
        }

    def signed_bytes(self) -> bytes:
        """The exact bytes the issuing root's signature covers."""
        return signed_bytes(CERTIFICATE_DOMAIN, self.canonical())


@dataclass(frozen=True)
class AnchorSet:
    """The verified anchor set: the trusted roots and the certificates they issued."""

    generation: int
    issued_at: int
    roots: tuple[Root, ...]
    certificates: tuple[Certificate, ...]
    signature: bytes
    sha256: str

    def root(self, key_id: str) -> Root | None:
        """The root with ``key_id``, if the set names it."""
        for root in self.roots:
            if root.key_id == key_id:
                return root
        return None

    def certificate(self, key_id: str) -> Certificate | None:
        """The certificate of ``key_id``, if the set holds one."""
        for certificate in self.certificates:
            if certificate.key_id == key_id:
                return certificate
        return None

    def signed_bytes(self) -> bytes:
        """The exact bytes the ``--root-key`` root's signature covers."""
        return signed_bytes(
            ANCHOR_SET_DOMAIN,
            {
                "certificates": [certificate.canonical() for certificate in self.certificates],
                "generation": self.generation,
                "issued_at": self.issued_at,
                "roots": [
                    {
                        "key_id": root.key_id,
                        "public_key": base64.b64encode(root.public_key).decode("ascii"),
                        "valid_from": root.valid_from,
                        "valid_to": root.valid_to,
                    }
                    for root in self.roots
                ],
            },
        )


@dataclass(frozen=True)
class RevokedKey:
    """A revoked signing key, with the instant from which its signatures are refused."""

    key_id: str
    effective_from: int
    compromised: bool
    reason: str


@dataclass(frozen=True)
class RevokedPublisher:
    """A revoked publisher, with the instant from which what it published is refused."""

    publisher_id: str
    effective_from: int
    reason: str


@dataclass(frozen=True)
class RevokedPackage:
    """A revoked package, by the digest of its signed manifest bytes."""

    digest: str
    reason: str


@dataclass(frozen=True)
class RevocationList:
    """The store's signed revocation list, as ``revocation.rs`` defines it."""

    generation: int
    issued_at: int
    issued_by: str
    keys: tuple[RevokedKey, ...]
    publishers: tuple[RevokedPublisher, ...]
    packages: tuple[RevokedPackage, ...]
    signature: bytes
    sha256: str

    def signed_bytes(self) -> bytes:
        """The exact bytes the issuing root's signature covers."""
        return signed_bytes(
            REVOCATION_DOMAIN,
            {
                "bundles": [{"digest": entry.digest, "reason": entry.reason} for entry in self.packages],
                "generation": self.generation,
                "issued_at": self.issued_at,
                "issued_by": self.issued_by,
                "keys": [
                    {
                        "compromised": entry.compromised,
                        "effective_from": entry.effective_from,
                        "key_id": entry.key_id,
                        "reason": entry.reason,
                    }
                    for entry in self.keys
                ],
                "publishers": [
                    {
                        "effective_from": entry.effective_from,
                        "publisher_id": entry.publisher_id,
                        "reason": entry.reason,
                    }
                    for entry in self.publishers
                ],
            },
        )


def parse_root_key(data: bytes) -> RootKey:
    """Reads the ``--root-key`` file: one root's public key, trusted out of band."""
    raw = _object(data, ROOT_KEY_MALFORMED, "root key file")
    _closed(raw, ROOT_KEY_MEMBERS, ROOT_KEY_MALFORMED, "root key file")
    _format(raw, ROOT_KEY_FORMAT, ROOT_KEY_MALFORMED, "root key file")
    key_id = _hex(raw.get("key_id"), 32, ROOT_KEY_MALFORMED, "root key file: key_id")
    public_key = _public_key(raw.get("public_key"), ROOT_KEY_MALFORMED, "root key file: public_key")
    _key_id_matches(public_key, key_id, ROOT_KEY_MALFORMED, "root key file")
    return RootKey(key_id=key_id, public_key=public_key)


def _parse_root(raw: object, index: int) -> Root:
    if not isinstance(raw, dict):
        raise _malformed(ANCHOR_SET_MALFORMED, f"anchor set: root #{index + 1} is an object")
    _closed(raw, ROOT_MEMBERS, ANCHOR_SET_MALFORMED, f"anchor set: root #{index + 1}")
    key_id = _hex(raw.get("key_id"), 32, ANCHOR_SET_MALFORMED, f"anchor set: root #{index + 1} key_id")
    public_key = _public_key(raw.get("public_key"), ANCHOR_SET_MALFORMED, f"anchor set: root #{index + 1} public_key")
    _key_id_matches(public_key, key_id, ANCHOR_SET_MALFORMED, f"anchor set: root #{index + 1}")
    valid_from = _integer(raw.get("valid_from"), ANCHOR_SET_MALFORMED, f"anchor set: root #{index + 1} valid_from")
    valid_to = _optional_integer(raw.get("valid_to"), ANCHOR_SET_MALFORMED, f"anchor set: root #{index + 1} valid_to")
    if valid_to is not None and valid_to <= valid_from:
        raise _malformed(ANCHOR_SET_MALFORMED, f"anchor set: root #{index + 1} valid_to is after valid_from")
    return Root(key_id=key_id, public_key=public_key, valid_from=valid_from, valid_to=valid_to)


def _parse_certificate(raw: object) -> Certificate:
    """Reads one certificate's shape. Its issuing root's signature is checked once the set is trusted."""
    if not isinstance(raw, dict):
        raise _malformed(ANCHOR_SET_MALFORMED, "anchor set: every certificate is an object")
    _closed(raw, CERTIFICATE_MEMBERS, ANCHOR_SET_MALFORMED, "anchor set: certificate")
    _format(raw, CERTIFICATE_FORMAT, ANCHOR_SET_MALFORMED, "anchor set: certificate")
    publisher_id = _text(raw.get("publisher_id"), ANCHOR_SET_MALFORMED, "certificate: publisher_id", limit=128)
    display_name = _text(raw.get("display_name"), ANCHOR_SET_MALFORMED, "certificate: display_name", limit=64)
    public_key = _public_key(raw.get("public_key"), ANCHOR_SET_MALFORMED, "certificate: public_key")
    valid_from = _integer(raw.get("valid_from"), ANCHOR_SET_MALFORMED, "certificate: valid_from")
    valid_to = _integer(raw.get("valid_to"), ANCHOR_SET_MALFORMED, "certificate: valid_to")
    if valid_to <= valid_from:
        raise _malformed(ANCHOR_SET_MALFORMED, "certificate: valid_to is after valid_from")
    return Certificate(
        publisher_id=publisher_id,
        display_name=display_name,
        public_key=public_key,
        valid_from=valid_from,
        valid_to=valid_to,
        issued_by=_hex(raw.get("issued_by"), 32, ANCHOR_SET_MALFORMED, "certificate: issued_by"),
        serial=_integer(raw.get("serial"), ANCHOR_SET_MALFORMED, "certificate: serial"),
        signature=_signature(raw.get("signature"), ANCHOR_SET_MALFORMED, "certificate: signature"),
    )


def _verify_certificate(certificate: Certificate, roots: tuple[Root, ...]) -> None:
    """The certificate chains to a root of the set, and that root signed it."""
    root = next((candidate for candidate in roots if candidate.key_id == certificate.issued_by), None)
    if root is None:
        raise Refused(
            UNKNOWN_ANCHOR,
            f"the certificate for '{certificate.publisher_id}' was issued by {certificate.issued_by}, "
            "which is not a root in the anchor set",
        )
    if not ed25519.verify(root.public_key, certificate.signed_bytes(), certificate.signature):
        raise Refused(
            CERTIFICATE_SIGNATURE_INVALID,
            f"the certificate for '{certificate.publisher_id}' does not verify under the root "
            f"{certificate.issued_by} that issued it",
        )


def parse_anchor_set(data: bytes, root_key: RootKey) -> AnchorSet:
    """Reads the root-signed anchor set and verifies it against ``root_key``."""
    raw = _object(data, ANCHOR_SET_MALFORMED, "anchor set")
    _closed(raw, ANCHOR_SET_MEMBERS, ANCHOR_SET_MALFORMED, "anchor set")
    _format(raw, ANCHOR_SET_FORMAT, ANCHOR_SET_MALFORMED, "anchor set")
    generation = _integer(raw.get("generation"), ANCHOR_SET_MALFORMED, "anchor set: generation")
    issued_at = _integer(raw.get("issued_at"), ANCHOR_SET_MALFORMED, "anchor set: issued_at")
    signature = _signature(raw.get("signature"), ANCHOR_SET_MALFORMED, "anchor set: signature")

    raw_roots = raw.get("roots")
    if not isinstance(raw_roots, list) or not raw_roots:
        raise _malformed(ANCHOR_SET_MALFORMED, "anchor set: roots is a non-empty array")
    roots = tuple(_parse_root(item, index) for index, item in enumerate(raw_roots))
    identifiers = [root.key_id for root in roots]
    if len(set(identifiers)) != len(identifiers):
        raise _malformed(ANCHOR_SET_MALFORMED, "anchor set: a root is listed twice")

    raw_certificates = raw.get("certificates")
    if not isinstance(raw_certificates, list):
        raise _malformed(ANCHOR_SET_MALFORMED, "anchor set: certificates is an array")
    certificates = tuple(_parse_certificate(item) for item in raw_certificates)
    key_ids = [certificate.key_id for certificate in certificates]
    if len(set(key_ids)) != len(key_ids):
        raise _malformed(ANCHOR_SET_MALFORMED, "anchor set: a publisher key is certified twice")

    anchor_set = AnchorSet(
        generation=generation,
        issued_at=issued_at,
        roots=roots,
        certificates=tuple(sorted(certificates, key=lambda certificate: certificate.key_id)),
        signature=signature,
        sha256=hashlib.sha256(data).hexdigest(),
    )
    # The signature is checked over every member it covers — the roots and every certificate's
    # identity — before anything in the document is believed.
    if not ed25519.verify(root_key.public_key, anchor_set.signed_bytes(), signature):
        raise Refused(
            ANCHOR_SET_SIGNATURE_INVALID,
            f"the anchor set does not verify under the root key {root_key.key_id} supplied by --root-key",
        )

    named = next((root for root in roots if root.key_id == root_key.key_id), None)
    if named is None:
        raise Refused(
            UNKNOWN_TRUST_ROOT,
            f"the anchor set is signed by {root_key.key_id}, but it does not name that root",
        )
    if named.public_key != root_key.public_key:
        raise _malformed(ANCHOR_SET_MALFORMED, f"anchor set: the entry for {root_key.key_id} is not that key")

    for certificate in anchor_set.certificates:
        _verify_certificate(certificate, roots)
    return anchor_set


def _parse_revoked_key(raw: object) -> RevokedKey:
    if not isinstance(raw, dict):
        raise _malformed(REVOCATION_MALFORMED, "revocation list: every revoked key is an object")
    _closed(raw, REVOKED_KEY_MEMBERS, REVOCATION_MALFORMED, "revocation list: revoked key")
    compromised = raw.get("compromised")
    if not isinstance(compromised, bool):
        raise _malformed(REVOCATION_MALFORMED, "revocation list: compromised is true or false")
    return RevokedKey(
        key_id=_hex(raw.get("key_id"), 32, REVOCATION_MALFORMED, "revocation list: key_id"),
        effective_from=_integer(raw.get("effective_from"), REVOCATION_MALFORMED, "revocation list: effective_from"),
        compromised=compromised,
        reason=_text(raw.get("reason"), REVOCATION_MALFORMED, "revocation list: reason", limit=REASON_LIMIT),
    )


def _parse_revoked_publisher(raw: object) -> RevokedPublisher:
    if not isinstance(raw, dict):
        raise _malformed(REVOCATION_MALFORMED, "revocation list: every revoked publisher is an object")
    _closed(raw, REVOKED_PUBLISHER_MEMBERS, REVOCATION_MALFORMED, "revocation list: revoked publisher")
    return RevokedPublisher(
        publisher_id=_text(raw.get("publisher_id"), REVOCATION_MALFORMED, "revocation list: publisher_id", limit=128),
        effective_from=_integer(raw.get("effective_from"), REVOCATION_MALFORMED, "revocation list: effective_from"),
        reason=_text(raw.get("reason"), REVOCATION_MALFORMED, "revocation list: reason", limit=REASON_LIMIT),
    )


def _parse_revoked_package(raw: object) -> RevokedPackage:
    if not isinstance(raw, dict):
        raise _malformed(REVOCATION_MALFORMED, "revocation list: every revoked package is an object")
    _closed(raw, REVOKED_PACKAGE_MEMBERS, REVOCATION_MALFORMED, "revocation list: revoked package")
    return RevokedPackage(
        digest=_hex(raw.get("digest"), 64, REVOCATION_MALFORMED, "revocation list: digest"),
        reason=_text(raw.get("reason"), REVOCATION_MALFORMED, "revocation list: reason", limit=REASON_LIMIT),
    )


def parse_revocations(data: bytes, anchor_set: AnchorSet, as_of: int) -> RevocationList:
    """Reads the root-signed revocation list and verifies it against a root of ``anchor_set``.

    The signing root must be one the verified anchor set names and must be trusted at ``as_of``: a
    list signed by a retired or unknown root says nothing about what this build may bundle.
    """
    raw = _object(data, REVOCATION_MALFORMED, "revocation list")
    _closed(raw, REVOCATION_MEMBERS, REVOCATION_MALFORMED, "revocation list")
    _format(raw, REVOCATION_FORMAT, REVOCATION_MALFORMED, "revocation list")

    lists: dict[str, list[Any]] = {}
    for name in ("keys", "publishers", "bundles"):
        value = raw.get(name)
        if not isinstance(value, list):
            raise _malformed(REVOCATION_MALFORMED, f"revocation list: {name} is an array")
        lists[name] = value
    keys, publishers, packages = lists["keys"], lists["publishers"], lists["bundles"]

    revocations = RevocationList(
        generation=_integer(raw.get("generation"), REVOCATION_MALFORMED, "revocation list: generation"),
        issued_at=_integer(raw.get("issued_at"), REVOCATION_MALFORMED, "revocation list: issued_at"),
        issued_by=_hex(raw.get("issued_by"), 32, REVOCATION_MALFORMED, "revocation list: issued_by"),
        keys=tuple(_parse_revoked_key(item) for item in keys),
        publishers=tuple(_parse_revoked_publisher(item) for item in publishers),
        packages=tuple(_parse_revoked_package(item) for item in packages),
        signature=_signature(raw.get("signature"), REVOCATION_MALFORMED, "revocation list: signature"),
        sha256=hashlib.sha256(data).hexdigest(),
    )

    root = anchor_set.root(revocations.issued_by)
    if root is None:
        raise Refused(
            UNKNOWN_TRUST_ROOT,
            f"the revocation list is signed by {revocations.issued_by}, which the anchor set does not name",
        )
    if not ed25519.verify(root.public_key, revocations.signed_bytes(), revocations.signature):
        raise Refused(
            REVOCATION_SIGNATURE_INVALID,
            f"the revocation list does not verify under the root {revocations.issued_by} it names",
        )
    if not root.valid_at(as_of):
        raise Refused(
            ROOT_OUTSIDE_WINDOW,
            f"the root {root.key_id} that signed the revocation list is not trusted at the build's --as-of instant",
        )
    return revocations


@dataclass(frozen=True)
class TrustMaterial:
    """The verified trust material and what a package must satisfy against it."""

    root_key_id: str
    anchor_set: AnchorSet
    revocations: RevocationList

    @property
    def digest(self) -> str:
        """The digest of the material a build used: both documents, by content."""
        return hashlib.sha256(
            signed_bytes(
                TRUST_MATERIAL_DOMAIN,
                {
                    "anchor_set_sha256": self.anchor_set.sha256,
                    "revocations_sha256": self.revocations.sha256,
                    "root_key_id": self.root_key_id,
                },
            )
        ).hexdigest()

    def check(self, package: Package, as_of: int) -> None:
        """Verifies one package against the material, at ``as_of``, refusing the first broken rule."""
        manifest = package.manifest

        certificate = self.anchor_set.certificate(manifest.key_id)
        if certificate is None:
            raise Refused(
                UNKNOWN_KEY,
                f"the signing key {manifest.key_id} is not named by a certificate in the store's anchor set",
            )
        if certificate.publisher_id != manifest.publisher_id:
            raise Refused(
                PUBLISHER_MISMATCH,
                f"the manifest says it was published by '{manifest.publisher_id}', but key {manifest.key_id} "
                f"belongs to '{certificate.publisher_id}'",
            )
        # The root that issued the certificate must be trusted at the build's instant: a certificate
        # signed by a retired root says nothing, even if the certificate's own window would cover.
        issuing_root = self.anchor_set.root(certificate.issued_by)
        if issuing_root is None or not issuing_root.valid_at(as_of):
            raise Refused(
                ROOT_OUTSIDE_WINDOW,
                f"the root {certificate.issued_by} that issued the certificate for '{certificate.publisher_id}' "
                f"is not trusted at the build's --as-of instant {as_of}",
            )
        if as_of < certificate.valid_from:
            raise Refused(
                CERTIFICATE_NOT_YET_VALID,
                f"the certificate for '{certificate.publisher_id}' is not valid before {certificate.valid_from}; "
                f"the build's --as-of instant is {as_of}",
            )
        if as_of >= certificate.valid_to:
            raise Refused(
                CERTIFICATE_EXPIRED,
                f"the certificate for '{certificate.publisher_id}' expired at {certificate.valid_to}; "
                f"the build's --as-of instant is {as_of}",
            )

        revocations = self.revocations
        for publisher in revocations.publishers:
            if publisher.publisher_id == manifest.publisher_id and publisher.effective_from <= as_of:
                raise Refused(
                    PUBLISHER_REVOKED,
                    f"the store revoked the publisher '{publisher.publisher_id}' from {publisher.effective_from}",
                )
        for key in revocations.keys:
            if key.key_id != manifest.key_id:
                continue
            if key.compromised:
                raise Refused(
                    KEY_COMPROMISED,
                    f"the store marked the signing key {key.key_id} compromised: every signature it made is refused",
                )
            if key.effective_from <= as_of:
                raise Refused(
                    KEY_REVOKED,
                    f"the store revoked the signing key {key.key_id} from {key.effective_from}",
                )
        digest = manifest.digest()
        for entry in revocations.packages:
            if entry.digest == digest:
                raise Refused(PACKAGE_REVOKED, f"the store revoked the package '{manifest.component_id}' by its digest")

        if not ed25519.verify(certificate.public_key, manifest.signed_bytes(), package.signature):
            raise Refused(
                SIGNATURE_INVALID,
                f"the signature does not verify over the canonical manifest of '{manifest.component_id}' "
                f"with key {manifest.key_id}",
            )


def require_generation(code: str, what: str, generation: int, minimum: int) -> None:
    """Refuses a signed document older than the pinned minimum (a replay of a genuine, older one).

    Called only after the document's signature has verified, so the number it judges is one the
    store signed. The pin comes from the command line, never from a file the document or the host
    supplies.
    """
    if generation < minimum:
        raise Refused(
            code,
            f"{what} has generation {generation}, below the pinned minimum {minimum}: "
            "an older signed document is not accepted (a replay)",
        )


def _pin(value: object, name: str) -> int:
    """A pin is a non-negative integer the caller passed explicitly; anything else fails closed."""
    if isinstance(value, bool) or not isinstance(value, int) or value < 0:
        raise ValueError(f"{name} is a non-negative integer")
    return value


def load_trust_material(
    *,
    anchor_set: bytes,
    revocations: bytes,
    root_key: bytes,
    as_of: int,
    min_anchors_generation: int,
    min_revocation_generation: int,
) -> TrustMaterial:
    """Verifies the three documents in order — root key, anchor set, revocation list — and returns them.

    Each document's signature is verified first, then its generation is compared with the pin. The
    pins have no default: a caller that does not pin cannot load material at all.
    """
    min_anchors = _pin(min_anchors_generation, "min_anchors_generation")
    min_revocations = _pin(min_revocation_generation, "min_revocation_generation")
    key = parse_root_key(root_key)
    verified_anchors = parse_anchor_set(anchor_set, key)
    require_generation(ANCHORS_STALE, "the anchor set", verified_anchors.generation, min_anchors)
    verified_revocations = parse_revocations(revocations, verified_anchors, as_of)
    require_generation(REVOCATION_STALE, "the revocation list", verified_revocations.generation, min_revocations)
    return TrustMaterial(
        root_key_id=key.key_id,
        anchor_set=verified_anchors,
        revocations=verified_revocations,
    )


def parse_instant(value: str) -> int:
    """``--as-of``: UTC seconds, or an ISO-8601 instant such as ``2026-10-26T00:00:00Z``."""
    text = value.strip()
    if text.isascii() and text.isdigit():
        return int(text)
    try:
        instant = datetime.fromisoformat(text.replace("Z", "+00:00"))
    except ValueError as error:
        raise Refused(
            "component.trust.instant_invalid",
            f"--as-of '{value}' is UTC seconds or an ISO-8601 instant such as 2026-10-26T00:00:00Z",
        ) from error
    if instant.tzinfo is None:
        raise Refused("component.trust.instant_invalid", f"--as-of '{value}' names no offset: write the Z")
    if instant.microsecond:
        raise Refused("component.trust.instant_invalid", f"--as-of '{value}' is a whole second")
    seconds = int(instant.timestamp())
    if seconds < 0:
        raise Refused("component.trust.instant_invalid", f"--as-of '{value}' is at or after 1970-01-01")
    return seconds


def parse_reviewed_list(text: str) -> tuple[str, ...]:
    """The digests of a reviewed-sets file: one SHA-256 per line, ``#`` comments and blanks ignored."""
    digests: list[str] = []
    for number, line in enumerate(text.splitlines(), start=1):
        entry = line.strip()
        if not entry or entry.startswith("#"):
            continue
        if len(entry) != 64 or any(character not in "0123456789abcdef" for character in entry):
            raise Refused(
                REVIEWED_LIST_MALFORMED,
                f"reviewed-sets line {number} is 64 lower-case hex characters",
            )
        digests.append(entry)
    return tuple(digests)


def reviewed_set_digest(data: bytes) -> str:
    """The digest a reviewed-sets file records for a set file: the SHA-256 of its bytes."""
    return hashlib.sha256(data).hexdigest()


def require_reviewed(digest: str, reviewed: Iterable[str], where: str) -> None:
    """Refuses a set file whose digest no reviewed-sets line records."""
    if digest not in tuple(reviewed):
        raise Refused(
            REVIEWED_SET_NOT_RECORDED,
            f"the set file's digest {digest} is not recorded in {where}: a reviewed set is the only one "
            "that may fall back to the keys in the set file",
        )


@dataclass(frozen=True)
class TrustRecord:
    """The build's record of the trust material it used, under a content-addressed name."""

    text: str
    sha256: str

    @property
    def file_name(self) -> str:
        """The name this record must be written under: its own digest, truncated."""
        return RECORD_PREFIX + self.sha256[:16] + RECORD_SUFFIX

    def write(self, directory: str) -> str:
        """Writes the record into ``directory`` and returns its path."""
        path = os.path.join(directory, self.file_name)
        with open(path, "w", encoding="utf-8", newline="") as handle:
            handle.write(self.text)
        return path


def _record_body(shell: str, components: Iterable[dict[str, str]], **members: Any) -> dict[str, Any]:
    body: dict[str, Any] = {
        "components": sorted(components, key=lambda entry: entry["component_id"]),
        "schema": RECORD_SCHEMA,
        "shell": shell,
    }
    body.update(members)
    return body


def _record(body: dict[str, Any]) -> TrustRecord:
    text = json.dumps(body, sort_keys=True, separators=(",", ":"), ensure_ascii=False)
    return TrustRecord(text=text, sha256=hashlib.sha256(text.encode("utf-8")).hexdigest())


def signed_record(
    *,
    shell: str,
    components: Iterable[dict[str, str]],
    set_manifest: str,
    material: TrustMaterial,
    as_of: int,
    min_anchors_generation: int,
    min_revocation_generation: int,
) -> TrustRecord:
    """The record of a build whose trust came from the store's signed material."""
    return _record(
        _record_body(
            shell,
            components,
            as_of=as_of,
            mode="signed",
            set_manifest=set_manifest,
            trust_material={
                "anchor_set_generation": material.anchor_set.generation,
                "anchor_set_sha256": material.anchor_set.sha256,
                "min_anchors_generation": min_anchors_generation,
                "min_revocation_generation": min_revocation_generation,
                "revocation_generation": material.revocations.generation,
                "revocations_sha256": material.revocations.sha256,
                "root_key_id": material.root_key_id,
            },
            trust_sha256=material.digest,
        )
    )


def reviewed_record(
    *,
    shell: str,
    components: Iterable[dict[str, str]],
    set_manifest: str,
    set_sha256: str,
    reviewed_list: str,
    reviewed_list_sha256: str,
) -> TrustRecord:
    """The record of a build that fell back to a reviewed set file's own keys."""
    return _record(
        _record_body(
            shell,
            components,
            mode="reviewed-set",
            set_manifest=set_manifest,
            trust_material={
                "reviewed_list": reviewed_list,
                "reviewed_list_sha256": reviewed_list_sha256,
                "reviewed_set_sha256": set_sha256,
            },
            trust_sha256=set_sha256,
        )
    )


def entry_of(component_id: str, component_version: str, manifest_sha256: str) -> dict[str, str]:
    """One component entry of a trust record: the identity and the digest that was bundled."""
    return {
        "component_id": component_id,
        "component_version": component_version,
        "manifest_sha256": manifest_sha256,
    }


def parse_record(text: str) -> dict[str, Any]:
    """Parses a trust record with the rules the tool writes it under, for the tests and the build log."""
    raw = json.loads(text)
    if not isinstance(raw, dict):
        raise ValueError("the trust record is a JSON object")
    members = {"as_of", "components", "mode", "schema", "set_manifest", "shell", "trust_material", "trust_sha256"}
    if raw.get("mode") == "reviewed-set":
        members = members - {"as_of"}
    unknown = set(raw) - members
    if unknown:
        raise ValueError("no such member as " + ", ".join(sorted(unknown)))
    if raw.get("schema") != RECORD_SCHEMA:
        raise ValueError(f"schema is {RECORD_SCHEMA}")
    if raw.get("mode") not in ("signed", "reviewed-set"):
        raise ValueError("mode is 'signed' or 'reviewed-set'")
    if not isinstance(raw.get("components"), list):
        raise ValueError("components is an array")
    for entry in raw["components"]:
        if not isinstance(entry, dict) or set(entry) != {"component_id", "component_version", "manifest_sha256"}:
            raise ValueError("every component entry is {component_id, component_version, manifest_sha256}")
    if not isinstance(raw.get("trust_material"), dict):
        raise ValueError("trust_material is an object")
    digest = raw.get("trust_sha256")
    if not isinstance(digest, str) or len(digest) != 64:
        raise ValueError("trust_sha256 is a SHA-256")
    return raw
