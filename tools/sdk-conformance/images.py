"""Parsing and validating ``tools/sdk-conformance/images.toml`` (WP-STR-07.T1).

The harness builds every language in a container image **pinned** by an immutable ``@sha256:``
digest or an explicit version tag. This module reads the file, checks the pinning rule and the
required fields, and hands ``run.py`` a plain structure. It never touches a container, so the
unit tests are pure.

A language may be added by adding a ``[languages.<name>]`` table with the four fields below and a
``sdk/templates/<name>/`` directory that builds ``out/component.wasm``; see
``sdk/templates/README.md``.
"""

from __future__ import annotations

import re
import tomllib
from dataclasses import dataclass
from pathlib import Path

#: The languages the harness ships entries for, in the order they are listed by default.
KNOWN_LANGUAGES = ("rust", "go", "csharp", "javascript", "python")

#: The fields every ``[languages.<name>]`` table must carry.
REQUIRED_FIELDS = ("image", "toolchain", "provision", "build")

_DIGEST = re.compile(r"@sha256:([0-9a-f]{64})$")


class ImagesError(ValueError):
    """The image configuration is malformed."""


@dataclass(frozen=True)
class HarnessConfig:
    """The ``[harness]`` table: where the runner, cases and component live."""

    runner: str
    cases: str
    component: str
    wasm_tools_version: str


@dataclass(frozen=True)
class LanguageImage:
    """One ``[languages.<name>]`` table."""

    language: str
    image: str
    toolchain: str
    provision: str
    build: str

    @property
    def digest(self) -> str | None:
        """The ``sha256:`` digest when the image is pinned by digest."""
        match = _DIGEST.search(self.image)
        return match.group(1) if match else None

    @property
    def tag(self) -> str:
        """The tag part of the reference (``''`` when there is none)."""
        reference = self.image.split("@", 1)[0]
        last = reference.rsplit("/", 1)[-1]
        return last.split(":", 1)[1] if ":" in last else ""

    @property
    def pinned(self) -> bool:
        """Whether the image is pinned by a full digest or an explicit, non-``latest`` tag."""
        if self.digest is not None:
            return True
        return bool(self.tag) and self.tag != "latest"


def is_pinned(reference: str) -> bool:
    """Whether an image reference is pinned (digest or a non-``latest`` version tag).

    ``is_pinned("rust")`` and ``is_pinned("rust:latest")`` are ``False``;
    ``is_pinned("rust:1.98-slim")`` and ``is_pinned("rust@sha256:<64 hex>")`` are ``True``.
    A digest that is not 64 lowercase hex characters is ``False``.
    """
    return LanguageImage(
        language="?", image=reference, toolchain="", provision="", build=""
    ).pinned


def _require_str(table: dict, key: str, where: str) -> str:
    value = table.get(key)
    if not isinstance(value, str) or not value.strip():
        raise ImagesError(f"{where}: `{key}` must be a non-empty string")
    return value


def load_images(path: str | Path) -> tuple[HarnessConfig, list[LanguageImage]]:
    """Reads and validates ``images.toml``.

    Returns the harness configuration and the language entries **in file order**. Raises
    :class:`ImagesError` for a missing section, a missing field, or an unpinned image.
    """
    path = Path(path)
    try:
        data = tomllib.loads(path.read_text(encoding="utf-8"))
    except FileNotFoundError as error:
        raise ImagesError(f"no image configuration at {path}") from error
    except tomllib.TOMLDecodeError as error:
        raise ImagesError(f"{path} is not valid TOML: {error}") from error

    harness = data.get("harness")
    if not isinstance(harness, dict):
        raise ImagesError("images.toml: missing [harness] table")
    config = HarnessConfig(
        runner=_require_str(harness, "runner", "[harness]"),
        cases=_require_str(harness, "cases", "[harness]"),
        component=_require_str(harness, "component", "[harness]"),
        wasm_tools_version=_require_str(harness, "wasm_tools_version", "[harness]"),
    )

    languages = data.get("languages")
    if not isinstance(languages, dict) or not languages:
        raise ImagesError("images.toml: missing [languages.<name>] tables")

    entries: list[LanguageImage] = []
    for name, table in languages.items():
        where = f"[languages.{name}]"
        if not isinstance(table, dict):
            raise ImagesError(f"{where} is not a table")
        entry = LanguageImage(
            language=name,
            image=_require_str(table, "image", where),
            toolchain=_require_str(table, "toolchain", where),
            provision=table.get("provision", ""),
            build=_require_str(table, "build", where),
        )
        if not isinstance(entry.provision, str):
            raise ImagesError(f"{where}: `provision` must be a string (empty when unused)")
        if not entry.pinned:
            raise ImagesError(
                f"{where}: image {entry.image!r} is not pinned; use an @sha256: digest or an "
                "explicit version tag (not `latest`)"
            )
        entries.append(entry)

    missing = [name for name in KNOWN_LANGUAGES if name not in languages]
    if missing:
        raise ImagesError(f"images.toml: missing the shipped language(s): {', '.join(missing)}")
    return config, entries


def by_language(entries: list[LanguageImage]) -> dict[str, LanguageImage]:
    """The entries keyed by language name."""
    return {entry.language: entry for entry in entries}
