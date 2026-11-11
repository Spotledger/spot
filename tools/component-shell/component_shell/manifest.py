"""The content-addressed manifest of a shell's component set.

``build-shell.py`` writes this document next to the components it bundles, and the generated shell
reads it at start-up to prove that the components it was compiled with are the components the build
approved. It is the same document the C# host reads
(``clients/avalonia/src/Spotledger.Component.Host/ComponentSetManifest.cs``), and the two are pinned to
one committed sample file so neither language can drift.

The rules, both sides:

* the file is named ``component-set-<hash>.json``, where ``<hash>`` is the first 16 lower-case hex
  characters of the SHA-256 of the file's own bytes — the name is a content address, so a manifest
  whose bytes do not hash to its name has been edited since the build;
* the document is closed: exactly ``schema``, ``shell`` and ``components``;
* ``components`` is sorted by ``component_id`` and lists each component once;
* each entry carries the identity the signature covered and two digests: the SHA-256 of the package's
  signed manifest bytes (what a revocation list names) and the SHA-256 of the assembly file the shell
  bundles.
"""

from __future__ import annotations

import hashlib
import json
import os
from dataclasses import dataclass
from typing import Any, Final, Iterable

#: The only schema version this tool writes and the host reads.
SCHEMA: Final = 1

#: The file name prefix of a content-hashed set manifest.
FILE_PREFIX: Final = "component-set-"

#: The file name suffix of a content-hashed set manifest.
FILE_SUFFIX: Final = ".json"

#: The characters of the content hash the file name carries.
HASH_CHARACTERS: Final = 16

#: The members a document has, and nothing else.
MEMBERS: Final = frozenset({"components", "schema", "shell"})

#: The members of an entry.
ENTRY_MEMBERS: Final = frozenset(
    {
        "component_id",
        "component_version",
        "entry_assembly_sha256",
        "key_id",
        "manifest_sha256",
        "publisher_id",
    }
)


class ManifestError(Exception):
    """The set manifest is not the document the host reads."""


@dataclass(frozen=True)
class Entry:
    """One bundled component: its signed identity and the digests of its bytes."""

    component_id: str
    component_version: str
    publisher_id: str
    key_id: str
    entry_assembly_sha256: str
    manifest_sha256: str

    def as_json(self) -> dict[str, Any]:
        """The entry as the document writes it."""
        return {
            "component_id": self.component_id,
            "component_version": self.component_version,
            "entry_assembly_sha256": self.entry_assembly_sha256,
            "key_id": self.key_id,
            "manifest_sha256": self.manifest_sha256,
            "publisher_id": self.publisher_id,
        }


def content_hash(data: bytes) -> str:
    """The SHA-256 of ``data``, lower-case hex."""
    return hashlib.sha256(data).hexdigest()


def file_name(digest: str) -> str:
    """The content-addressed file name for a SHA-256."""
    if len(digest) < HASH_CHARACTERS:
        raise ManifestError("a content hash is at least 16 hex characters")
    return FILE_PREFIX + digest[:HASH_CHARACTERS] + FILE_SUFFIX


def document(shell: str, entries: Iterable[Entry]) -> str:
    """The manifest text: canonical JSON, entries sorted by id, no trailing newline."""
    if not shell:
        raise ManifestError("the shell has a name")
    ordered = sorted(entries, key=lambda entry: entry.component_id)
    identifiers = [entry.component_id for entry in ordered]
    if len(set(identifiers)) != len(identifiers):
        raise ManifestError("a component is listed twice")
    body = {
        "components": [entry.as_json() for entry in ordered],
        "schema": SCHEMA,
        "shell": shell,
    }
    # Sorted keys and no whitespace, exactly as the C# reader's canonical form and as the rest of the
    # signing scheme write JSON. json.dumps with sort_keys is the same encoding for this document:
    # every value is a string, an integer or a list of flat objects, and nothing needs escaping beyond
    # what json.dumps does for ASCII text.
    return json.dumps(body, sort_keys=True, separators=(",", ":"), ensure_ascii=False)


@dataclass(frozen=True)
class SetManifest:
    """A manifest and the digest of its own bytes."""

    shell: str
    entries: tuple[Entry, ...]
    text: str
    sha256: str

    @property
    def file_name(self) -> str:
        """The content-addressed name this manifest must be written under."""
        return file_name(self.sha256)

    def write(self, directory: str) -> str:
        """Writes the manifest into ``directory`` under its content-addressed name."""
        os.makedirs(directory, exist_ok=True)
        path = os.path.join(directory, self.file_name)
        with open(path, "w", encoding="utf-8", newline="") as handle:
            handle.write(self.text)
        return path


def build(shell: str, entries: Iterable[Entry]) -> SetManifest:
    """Builds a manifest and records the digest of the bytes it will be written as."""
    text = document(shell, entries)
    ordered = tuple(sorted(entries, key=lambda entry: entry.component_id))
    return SetManifest(shell=shell, entries=ordered, text=text, sha256=content_hash(text.encode("utf-8")))


def parse(text: str) -> SetManifest:
    """Parses a manifest with the host's rules, so a tool bug cannot ship a document it rejects."""
    try:
        raw = json.loads(text)
    except json.JSONDecodeError as error:
        raise ManifestError(f"the set manifest is not JSON: {error}") from error
    if not isinstance(raw, dict):
        raise ManifestError("the set manifest is a JSON object")
    unknown = set(raw) - MEMBERS
    if unknown:
        raise ManifestError("no such member as " + ", ".join(sorted(unknown)))
    if raw.get("schema") != SCHEMA:
        raise ManifestError(f"schema is {SCHEMA}")
    shell = raw.get("shell")
    if not isinstance(shell, str) or not shell:
        raise ManifestError("shell is a non-empty string")
    components = raw.get("components")
    if not isinstance(components, list):
        raise ManifestError("components is an array")

    entries: list[Entry] = []
    for item in components:
        if not isinstance(item, dict):
            raise ManifestError("every component entry is an object")
        unknown = set(item) - ENTRY_MEMBERS
        if unknown:
            raise ManifestError("no such entry member as " + ", ".join(sorted(unknown)))
        values = {}
        for name in sorted(ENTRY_MEMBERS):
            value = item.get(name)
            if not isinstance(value, str) or not value:
                raise ManifestError(f"entry {name} is a non-empty string")
            values[name] = value
        for name in ("entry_assembly_sha256", "manifest_sha256"):
            if len(values[name]) != 64 or any(c not in "0123456789abcdef" for c in values[name]):
                raise ManifestError(f"entry {name} is 64 lower-case hex characters")
        if len(values["key_id"]) != 32 or any(c not in "0123456789abcdef" for c in values["key_id"]):
            raise ManifestError("entry key_id is 32 lower-case hex characters")
        entries.append(Entry(**values))

    identifiers = [entry.component_id for entry in entries]
    if identifiers != sorted(identifiers):
        raise ManifestError("the entries are sorted by component_id")
    if len(set(identifiers)) != len(identifiers):
        raise ManifestError("a component is listed twice")
    return SetManifest(
        shell=shell,
        entries=tuple(entries),
        text=text,
        sha256=content_hash(text.encode("utf-8")),
    )
