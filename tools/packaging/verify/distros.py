"""The three distributions the packaging suite proves, and what each package promises.

The table is data: a new distribution is a new :class:`Distro` here, a Containerfile under
``harness/packaging-linux/<name>/`` and nothing else. Every distro carries a
:class:`Policy` that states what *its* packages do — an RPM erase and a DEB purge do not
behave the same way, and the scenarios assert the stated behaviour instead of a guess.
"""

from __future__ import annotations

from dataclasses import dataclass
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[3]
HARNESS_DIR = REPO_ROOT / "harness" / "packaging-linux"
PACKAGING_DIR = REPO_ROOT / "packaging" / "linux"

RPM = "rpm"
DEB = "deb"

#: How long the health endpoint has to answer after ``systemctl start`` (seconds).
HEALTH_TIMEOUT_S = 60
#: Poll interval while waiting for the health endpoint (seconds).
HEALTH_POLL_S = 2
#: How long systemd inside the container has to reach ``running`` (seconds).
SYSTEMD_READY_S = 180
#: Poll interval while waiting for systemd (seconds).
SYSTEMD_POLL_S = 2
#: How long a killed process has to be back, honouring ``RestartSec=`` (seconds).
RESTART_BOUND_S = 30
#: The container image's HEALTHCHECK budget: start-period plus retries (seconds).
IMAGE_HEALTH_S = 120
#: How long SIGTERM has to end the container process (seconds).
IMAGE_SIGTERM_GRACE_S = 30
#: ``systemctl stop`` gets the unit's own ``TimeoutStopSec`` plus this margin (seconds).
STOP_MARGIN_S = 5


@dataclass(frozen=True)
class Policy:
    """What one distribution's packages promise about an upgrade and a removal.

    These are readings of the shipped packaging in ``packaging/linux`` (the RPM spec and
    the DEB maintainer scripts), not preferences: the scenarios assert them, so a change
    to the packaging that breaks a promise fails the run instead of passing silently.
    """

    #: A running service is restarted by the package's own scriptlets on upgrade.
    restart_on_upgrade: bool
    #: Why the restart does or does not happen, in one line for the report and the docs.
    restart_reason: str
    #: A plain ``remove``/``erase`` leaves ``/etc/spot/spot.toml`` in place.
    remove_keeps_config: bool
    #: Suffix of the backup file a modified config is saved as, or ``""`` for none.
    config_backup_suffix: str
    #: Removal has a second step (DEB: purge) rather than one (RPM: erase).
    separate_purge: bool
    #: Removal deletes the ``spot`` service account.
    removes_service_user: bool
    #: What installing an older package over a newer one does, in one line.
    downgrade_expectation: str


# Where each upgrade restart comes from, so a change to either script finds this file:
#   RPM (Fedora, AlmaLinux): the spec's %postun runs %systemd_postun_with_restart.
#   DEB (Ubuntu): the postinst runs one `systemctl try-restart spot.service` on upgrade.
FEDORA_POLICY = Policy(
    restart_on_upgrade=True,
    restart_reason=(
        "%postun runs %systemd_postun_with_restart, which restarts a service that was "
        "active before the upgrade"
    ),
    remove_keeps_config=False,
    config_backup_suffix=".rpmsave",
    separate_purge=False,
    removes_service_user=False,
    downgrade_expectation=(
        "dnf install of an older .rpm reports the installed package and leaves the newer "
        "version in place; an operator downgrades explicitly with dnf downgrade"
    ),
)

ALMALINUX_POLICY = Policy(
    restart_on_upgrade=True,
    restart_reason=(
        "%postun runs %systemd_postun_with_restart, which restarts a service that was "
        "active before the upgrade"
    ),
    remove_keeps_config=False,
    config_backup_suffix=".rpmsave",
    separate_purge=False,
    removes_service_user=False,
    downgrade_expectation=(
        "dnf install of an older .rpm reports the installed package and leaves the newer "
        "version in place; an operator downgrades explicitly with dnf downgrade"
    ),
)

UBUNTU_POLICY = Policy(
    restart_on_upgrade=True,
    restart_reason=(
        "the postinst (packaging/linux/debian/postinst) runs one systemctl try-restart "
        "spot.service on an upgrade of a running service; the .deb that "
        "tools/packaging/build.py builds does not run debhelper, so the restart comes from "
        "the maintainer script itself, not from #DEBHELPER#"
    ),
    remove_keeps_config=True,
    config_backup_suffix="",
    separate_purge=True,
    removes_service_user=False,
    downgrade_expectation=(
        "apt-get install of an older .deb refuses the downgrade and exits non-zero, "
        "leaving the newer version installed; --allow-downgrades is the explicit override"
    ),
)


@dataclass(frozen=True)
class Distro:
    """One target distribution: how to install on it and what its packages promise."""

    name: str
    family: str
    package_format: str
    package_manager: str
    base_image: str
    image_tag: str
    unit_path: str
    config_path: str
    environment_path: str
    data_dir: str
    user: str
    group: str
    health_url: str
    policy: Policy
    unit_name: str = "spot.service"

    @property
    def containerfile(self) -> Path:
        """The Containerfile that builds this distro's systemd test image."""
        return HARNESS_DIR / self.name / "Containerfile"

    @property
    def container_name(self) -> str:
        """The deterministic podman container name for this distro."""
        return f"spot-pkgverify-{self.name}"

    @property
    def is_rpm(self) -> bool:
        """Whether this distro installs RPM packages."""
        return self.package_format == RPM


FEDORA_44 = Distro(
    name="fedora-44",
    family="fedora",
    package_format=RPM,
    package_manager="dnf",
    base_image="registry.fedoraproject.org/fedora:44",
    image_tag="localhost/spot-pkgverify-fedora-44:44",
    unit_path="/usr/lib/systemd/system/spot.service",
    config_path="/etc/spot/spot.toml",
    environment_path="/etc/spot/environment",
    data_dir="/var/lib/spot",
    user="spot",
    group="spot",
    health_url="http://127.0.0.1:8000/api/health/live",
    policy=FEDORA_POLICY,
)

ALMALINUX_10 = Distro(
    name="almalinux-10",
    family="rhel",
    package_format=RPM,
    package_manager="dnf",
    base_image="quay.io/almalinuxorg/almalinux:10",
    image_tag="localhost/spot-pkgverify-almalinux-10:10",
    unit_path="/usr/lib/systemd/system/spot.service",
    config_path="/etc/spot/spot.toml",
    environment_path="/etc/spot/environment",
    data_dir="/var/lib/spot",
    user="spot",
    group="spot",
    health_url="http://127.0.0.1:8000/api/health/live",
    policy=ALMALINUX_POLICY,
)

UBUNTU_2404 = Distro(
    name="ubuntu-2404",
    family="ubuntu",
    package_format=DEB,
    package_manager="apt-get",
    base_image="docker.io/library/ubuntu:24.04",
    image_tag="localhost/spot-pkgverify-ubuntu-2404:24.04",
    unit_path="/usr/lib/systemd/system/spot.service",
    config_path="/etc/spot/spot.toml",
    environment_path="/etc/spot/environment",
    data_dir="/var/lib/spot",
    user="spot",
    group="spot",
    health_url="http://127.0.0.1:8000/api/health/live",
    policy=UBUNTU_POLICY,
)

#: Every distro the suite proves, in the order it runs them.
DISTROS: tuple[Distro, ...] = (FEDORA_44, ALMALINUX_10, UBUNTU_2404)

#: The names accepted on the command line, plus ``all``.
DISTRO_NAMES: tuple[str, ...] = tuple(distro.name for distro in DISTROS)
ALL = "all"
CHOICES: tuple[str, ...] = (*DISTRO_NAMES, ALL)


def get(name: str) -> Distro:
    """The distro called ``name``.

    Raises:
        KeyError: when no distro has that name.
    """
    for distro in DISTROS:
        if distro.name == name:
            return distro
    raise KeyError(f"unknown distro: {name}")


def select(name: str) -> tuple[Distro, ...]:
    """The distros selected by ``name``: one of :data:`DISTRO_NAMES` or ``all``."""
    if name == ALL:
        return DISTROS
    return (get(name),)
