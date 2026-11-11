"""The artefacts ``tools/packaging/build.py`` wrote, read from a directory.

The driver builds nothing: it looks in ``--package-dir`` for the ``.rpm`` and ``.deb`` files
the build step produced, reads the version out of each file name (that is where
``build.py`` puts it), and orders them so "version N" and "version N+1" are unambiguous.
A pre-release sorts before the final release, matching the RPM ``Release=0.rc`` and the
Debian ``~rc`` mapping in ``build.py``.
"""

from __future__ import annotations

import re
from dataclasses import dataclass
from pathlib import Path

from distros import DEB, RPM

#: ``spot-1.2.3-1.fc44.x86_64.rpm``: name (no dash), version (no dash), release, arch.
RPM_NAME = re.compile(
    r"^(?P<name>[a-z0-9][a-z0-9+._]*?)-(?P<version>[0-9][^-]*)-"
    r"(?P<release>[^-]+)\.(?P<arch>[A-Za-z0-9_]+)\.rpm$"
)
#: ``spot_1.2.3_amd64.deb``: name (no underscore), version, Debian architecture.
DEB_NAME = re.compile(
    r"^(?P<name>[a-z0-9][a-z0-9+.-]*)_(?P<version>[^_]+)_(?P<arch>[a-z0-9]+)\.deb$"
)

#: The package name every artefact must carry; anything else is not ours.
PACKAGE_NAME = "spot"


@dataclass(frozen=True)
class Artefact:
    """One installable package file and the version it carries."""

    path: Path
    package_format: str
    name: str
    version: str
    release: str = ""
    arch: str = ""

    @property
    def filename(self) -> str:
        """The file name, without its directory."""
        return self.path.name

    @property
    def display_version(self) -> str:
        """The version as the package manager reports it (``1.2.3-1`` for RPM)."""
        if self.package_format == RPM and self.release:
            return f"{self.version}-{self.release}"
        return self.version

    @property
    def key(self) -> tuple:
        """A sort key: the upstream version, then the release."""
        return (version_key(self.version), release_key(self.release))


def _numbers(value: str) -> tuple[int, ...]:
    """The digit runs of ``value``, as integers, so ``1.2.3`` sorts numerically."""
    return tuple(int(part) for part in re.findall(r"\d+", value))


def version_key(version: str) -> tuple:
    """A comparable key for a package version; a pre-release sorts before the release.

    ``1.2.3`` and ``1.2.4`` compare numerically; ``1.2.3~rc.1`` (Debian) and ``1.2.3-rc.1``
    (RPM) both sort before ``1.2.3``, which is the order the packages install in.
    """
    value = version.strip()
    for separator in ("~", "-"):
        if separator in value:
            base, _, pre = value.partition(separator)
            return (_numbers(base), 0, pre)
    return (_numbers(value), 1, "")


def release_key(release: str) -> tuple:
    """A comparable key for an RPM release such as ``1`` or ``0.rc.1``."""
    if not release:
        return (1, "")
    if release.startswith("0."):
        return (0, release)
    return (1, release)


def parse(path: Path) -> Artefact | None:
    """The artefact ``path`` describes, or ``None`` when it is not one of our packages.

    ``None`` covers a different package name, a source RPM, a ``.rpmnew``/``.dpkg-dist``
    backup and anything else the directory happens to hold.
    """
    name = path.name
    if path.suffix == ".rpm" and not name.endswith(".src.rpm"):
        match = RPM_NAME.match(name)
        if match and match.group("name") == PACKAGE_NAME:
            return Artefact(
                path=path,
                package_format=RPM,
                name=match.group("name"),
                version=match.group("version"),
                release=match.group("release"),
                arch=match.group("arch"),
            )
        return None
    if path.suffix == ".deb":
        match = DEB_NAME.match(name)
        if match and match.group("name") == PACKAGE_NAME:
            return Artefact(
                path=path,
                package_format=DEB,
                name=match.group("name"),
                version=match.group("version"),
                arch=match.group("arch"),
            )
    return None


def discover(directory: Path | None) -> tuple[Artefact, ...]:
    """Every ``spot`` artefact in ``directory``, oldest version first.

    A missing directory is not an error: it means the build step has not run, and the
    scenarios that need an artefact report a skip with that reason.
    """
    if directory is None:
        return ()
    if not directory.exists():
        return ()
    found = [parse(path) for path in sorted(directory.iterdir())]
    artefacts = [artefact for artefact in found if artefact is not None]
    return tuple(sorted(artefacts, key=lambda artefact: artefact.key))


def for_format(artefacts: tuple[Artefact, ...], package_format: str) -> tuple[Artefact, ...]:
    """Only the artefacts of one package format (``rpm`` or ``deb``)."""
    return tuple(artefact for artefact in artefacts if artefact.package_format == package_format)


def latest(artefacts: tuple[Artefact, ...], package_format: str) -> Artefact | None:
    """The newest artefact of ``package_format``, or ``None``."""
    matching = for_format(artefacts, package_format)
    return matching[-1] if matching else None


def upgrade_pair(
    artefacts: tuple[Artefact, ...], package_format: str
) -> tuple[Artefact, Artefact] | None:
    """The oldest and newest artefact of one format, when they differ.

    Returns ``None`` when fewer than two versions are present, which is what the upgrade
    scenario turns into a skip: one package cannot prove an upgrade.
    """
    matching = for_format(artefacts, package_format)
    if len(matching) < 2:
        return None
    oldest, newest = matching[0], matching[-1]
    if oldest.version == newest.version and oldest.release == newest.release:
        return None
    return oldest, newest
