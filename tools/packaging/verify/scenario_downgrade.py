"""Scenario: installing an older package over a newer one.

The two families differ and the table states each: ``dnf install`` of an older ``.rpm``
reports what is installed and leaves the newer version alone, while ``apt-get install`` of an
older ``.deb`` refuses the downgrade and exits non-zero. Both are "no downgrade without an
explicit operator action", which is the invariant this scenario asserts: after the attempt
the installed version is still N+1, and the exit code and first output line are recorded so
the actual behaviour of each package manager is visible in the report.
"""

from __future__ import annotations

import commands
from context import Context, Step
from distros import Distro

NAME = "downgrade"
PROVES = (
    "installing version N over an installed N+1 does not silently downgrade; the package "
    "manager either refuses (DEB) or leaves N+1 in place (RPM), as the table states"
)
REQUIRES = ("upgrade",)

#: How much of the package manager's answer the detail keeps.
DETAIL_CHARS = 120


def plan(distro: Distro, package: str = "", newer: str = "") -> tuple[Step, ...]:
    """The commands this scenario runs, for ``--dry-run``."""
    del newer
    return (
        Step(
            "installed version",
            tuple(commands.installed_version(distro)),
            "version N+1, the newer artefact",
        ),
        Step(
            "install N over N+1",
            tuple(commands.install(package or "<package N>", distro)),
            distro.policy.downgrade_expectation,
        ),
        Step(
            "installed version",
            tuple(commands.installed_version(distro)),
            "still version N+1 (no silent downgrade)",
        ),
    )


def run(context: Context) -> str:
    """Attempt the downgrade and assert the version did not move; returns a one-line detail."""
    distro = context.distro
    old, new = context.pair()

    installed = context.installed_version()
    context.check(
        installed == new.display_version,
        f"the installed version is {installed!r}, expected {new.display_version!r}",
    )

    attempt = context.exec(commands.install(context.container_path(old), distro))
    answer = attempt.first_line()[:DETAIL_CHARS]

    after = context.installed_version()
    context.check(
        after == new.display_version,
        f"installing {old.display_version} over {new.display_version} changed the "
        f"installed version to {after!r}: a silent downgrade",
    )

    return (
        f"installing {old.display_version} over {new.display_version} exited "
        f"{attempt.returncode} and left {after} installed"
        + (f" ({answer})" if answer else "")
    )
