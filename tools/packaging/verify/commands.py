"""Every command line the scenarios run, as an argument list.

Nothing here is a shell string: each function returns ``list[str]`` and the driver passes it
straight to ``subprocess`` (or to ``podman exec``, which appends it as the container's
command). A package path is therefore one element, whatever characters it contains — a
space or a semicolon in an artefact name stays inside that element and is never parsed by a
shell. The unit tests assert exactly that.
"""

from __future__ import annotations

from distros import DEB, Distro

#: Shells that cannot be used to log in, so an account with one has no login shell.
NOLOGIN_SHELLS = frozenset({"/sbin/nologin", "/usr/sbin/nologin", "/bin/false", "/usr/bin/false"})

#: Run apt non-interactively: a .deb postinst must never wait for a prompt.
APT_ENV = ("env", "DEBIAN_FRONTEND=noninteractive")

#: Do not pull `Recommends:` (postgresql-18) into a throwaway container.
APT_INSTALL_FLAGS = ("-y", "--no-install-recommends", "install")


def install(package_path: str, distro: Distro) -> list[str]:
    """Install one local package file (its absolute path inside the container)."""
    if distro.package_format == DEB:
        return [*APT_ENV, "apt-get", *APT_INSTALL_FLAGS, package_path]
    return ["dnf", "-y", "install", package_path]


def remove(distro: Distro) -> list[str]:
    """Remove the package, keeping the configuration where the distribution does."""
    if distro.package_format == DEB:
        return [*APT_ENV, "apt-get", "-y", "remove", "spot"]
    return ["dnf", "-y", "remove", "spot"]


def purge(distro: Distro) -> list[str]:
    """Purge the package, deleting its configuration files (DEB) or erasing it (RPM)."""
    if distro.package_format == DEB:
        return [*APT_ENV, "apt-get", "-y", "purge", "spot"]
    return ["dnf", "-y", "remove", "spot"]


def is_installed(distro: Distro) -> list[str]:
    """Query the package's install state.

    RPM exits non-zero when the package is absent. DEB does not: ``dpkg-query -W`` succeeds
    and prints the state word, because a removed package stays in ``config-files`` until it
    is purged, so the state has to be read (see :func:`package_is_installed`).
    """
    if distro.package_format == DEB:
        return ["dpkg-query", "-W", "-f", "${Status}", "spot"]
    return ["rpm", "-q", "spot"]


def package_is_installed(distro: Distro, stdout: str) -> bool:
    """Whether the output of :func:`is_installed` says the package is installed.

    DEB prints ``install ok installed``, or ``deinstall ok config-files`` once the files
    are gone but the conffiles remain; RPM prints ``name-version-release`` or nothing.
    """
    if distro.package_format == DEB:
        return "install ok installed" in stdout
    return bool(stdout.strip())


def installed_version(distro: Distro) -> list[str]:
    """Print the installed version, in the distribution's own spelling."""
    if distro.package_format == DEB:
        return ["dpkg-query", "-W", "-f", "${Version}", "spot"]
    return ["rpm", "-q", "--qf", "%{VERSION}-%{RELEASE}", "spot"]


def config_files(distro: Distro) -> list[str]:
    """List the package's config files with, for RPM, the flags that mark ``noreplace``."""
    if distro.package_format == DEB:
        return ["dpkg-query", "-W", "-f", "${Conffiles}\n", "spot"]
    return ["rpm", "-q", "--qf", "[%{FILENAMES} %{FILEFLAGS:fflags}\n]", "spot"]


def unit_show(*properties: str, unit: str = "spot.service") -> list[str]:
    """``systemctl show`` for the named properties of the unit."""
    argv = ["systemctl", "show"]
    for property_name in properties:
        argv += ["-p", property_name]
    return [*argv, unit]


def start(unit: str = "spot.service") -> list[str]:
    """Start the unit explicitly, which is what an administrator does after setup."""
    return ["systemctl", "start", unit]


def stop(unit: str = "spot.service") -> list[str]:
    """Stop the unit; the call has to return inside the unit's ``TimeoutStopSec``."""
    return ["systemctl", "stop", unit]


def is_enabled(unit: str = "spot.service") -> list[str]:
    """Query the enablement state; exit 1 and print ``disabled`` when it is not enabled."""
    return ["systemctl", "is-enabled", unit]


def is_active(unit: str = "spot.service") -> list[str]:
    """Query the activity state; exit 3 and print ``inactive``/``failed`` when stopped."""
    return ["systemctl", "is-active", unit]


def verify_unit(unit_path: str) -> list[str]:
    """``systemd-analyze verify`` on the installed unit file."""
    return ["systemd-analyze", "verify", "--no-pager", unit_path]


def kill(pid: str) -> list[str]:
    """Send ``SIGKILL`` to the main process, to prove the unit restarts it."""
    return ["kill", "-9", pid]


def health(url: str) -> list[str]:
    """Fetch the health endpoint, failing on a non-2xx answer or a timeout."""
    return ["curl", "-fsS", "--max-time", "3", url]


def stat(fmt: str, path: str) -> list[str]:
    """``stat`` with an explicit format, so the output is parsed, not scraped."""
    return ["stat", "-c", fmt, path]


def getent(database: str, key: str) -> list[str]:
    """Look an account or a group up in the NSS database."""
    return ["getent", database, key]


def cat(path: str) -> list[str]:
    """Print a file, so a scenario can assert on its content."""
    return ["cat", path]


def exists(path: str) -> list[str]:
    """``test -e``: exit 0 when the path exists, 1 when it does not."""
    return ["test", "-e", path]


def install_file(source: str, destination: str, *, mode: str, owner: str, group: str) -> list[str]:
    """Copy a file from the read-only work mount into place with an exact mode and owner."""
    return ["install", "-m", mode, "-o", owner, "-g", group, source, destination]


def write_file(source: str, destination: str) -> list[str]:
    """Copy a fixture into place without changing its mode or owner."""
    return ["cp", source, destination]


def id_user() -> list[str]:
    """The numeric uid of the process the container runs as."""
    return ["id", "-u"]


def parse_show(stdout: str) -> dict[str, str]:
    """``systemctl show`` output as a mapping of property to value."""
    values: dict[str, str] = {}
    for line in stdout.splitlines():
        key, separator, value = line.partition("=")
        if not separator:
            continue
        values[key.strip()] = value.strip()
    return values


def parse_microseconds(value: str) -> int:
    """A systemd ``...USec`` value as whole seconds; ``0`` (infinity) stays ``0``."""
    try:
        return int(value, 10) // 1_000_000
    except (TypeError, ValueError):
        return 0


def login_shell_is_disabled(shell: str) -> bool:
    """Whether ``shell`` is one of the shells that refuse a login."""
    return shell.strip() in NOLOGIN_SHELLS


def passwd_fields(stdout: str) -> tuple[str, ...]:
    """The colon-separated fields of a ``getent passwd`` line."""
    line = stdout.strip().splitlines()[0] if stdout.strip() else ""
    return tuple(line.split(":"))


def config_is_noreplace(distro: Distro, stdout: str, config_path: str) -> bool:
    """Whether the package metadata marks ``config_path`` as ``noreplace``/a conffile.

    RPM prints one ``path flags`` line per file, where the flags include ``n`` for
    ``%config(noreplace)``. DEB prints the ``Conffiles:`` stanza, whose lines name the file
    and its recorded digest; a file listed there is a conffile.
    """
    for line in stdout.splitlines():
        stripped = line.strip()
        if not stripped.startswith(config_path):
            continue
        if distro.package_format == DEB:
            return True
        parts = stripped.split()
        if len(parts) >= 2 and "n" in parts[1]:
            return True
    return False
