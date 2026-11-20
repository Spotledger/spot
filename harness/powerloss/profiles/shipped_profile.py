"""The shipped-host profiles: what runs, what is cut and how (WP-PRD-05.T2).

A profile targets the **real shipped host**: ``spot serve`` and the bundled PostgreSQL
(``spot-pg-supervisor``), or on Windows the ``SpotledgerServer`` service (``spot-winsvc``).
Three profiles:

``linux-process-kill``
    ``kill -9`` on ``spot serve`` and the postmaster, on a disposable Linux host. Runs on a
    runner; the weakest cut (the page cache survives), the CI-grade check.
``linux-vm-power-cut``
    A hypervisor hard reset (libvirt ``virsh destroy``) of a disposable Linux VM that runs the
    shipped host. The real approximation of a power cut.
``windows-vm``
    A forced stop (Hyper-V ``Stop-VM -TurnOff``) of a disposable Windows VM that runs the
    service. Run on the Windows VM runner.

A profile is data: names, drivers and the argv lists of the adapters. Operational names (the
VM, the hosts, the paths) come from flags or the environment, never from this file, and every
name is validated before it reaches a command (``cutdrivers.safe_token``).
"""

from __future__ import annotations

import os
import shlex
import sys
from dataclasses import dataclass
from typing import Mapping

from cutdrivers import safe_token

HERE = os.path.dirname(os.path.abspath(__file__))
ADAPTERS = os.path.join(HERE, "shipped_adapters.py")
GUEST_WORKLOAD = os.path.join(os.path.dirname(HERE), "guest", "run_workload.py")

SERVICE_NAME = "SpotledgerServer"  # spot-winsvc `SERVICE_NAME`
DEFAULT_HEALTH_ADDR = "127.0.0.1:8080"  # spot-winsvc default `health_addr`
CLIENT_CMD_ENV = "SPOT_POWERLOSS_CLIENT_CMD"
START_CMD_ENV = "SPOT_POWERLOSS_START_CMD"

#: Cycles of the full acceptance run and of a dry run.
ACCEPTANCE_CYCLES = 200
DRY_RUN_CYCLES = 6


class ProfileError(Exception):
    """The profile is not runnable as configured."""


@dataclass(frozen=True)
class ShippedProfile:
    """One crash-consistency profile of the shipped host."""

    name: str
    description: str
    os_name: str  # "linux" | "windows": the OS of the *harness host* that issues the cut
    guest_os: str  # the OS of the machine that runs the shipped host
    driver: str  # a name from cutdrivers.DRIVERS
    target_kind: str  # "local" | "vm"
    hypervisor: str = ""
    ssh_guest: bool = False
    cycles: int = ACCEPTANCE_CYCLES


PROFILES: dict[str, ShippedProfile] = {
    p.name: p
    for p in (
        ShippedProfile(
            name="linux-process-kill",
            description="kill -9 of `spot serve` and the bundled PostgreSQL on a disposable Linux host.",
            os_name="linux",
            guest_os="linux",
            driver="process-kill",
            target_kind="local",
        ),
        ShippedProfile(
            name="linux-vm-power-cut",
            description="Hypervisor hard reset (virsh destroy) of a disposable Linux VM running the shipped host.",
            os_name="linux",
            guest_os="linux",
            driver="vm-reset",
            target_kind="vm",
            hypervisor="libvirt",
            ssh_guest=True,
        ),
        ShippedProfile(
            name="windows-vm",
            description="Forced stop (Stop-VM -TurnOff) of a disposable Windows VM running the SpotledgerServer service.",
            os_name="windows",
            guest_os="windows",
            driver="vm-reset",
            target_kind="vm",
            hypervisor="hyperv",
            ssh_guest=True,
        ),
    )
}


@dataclass(frozen=True)
class HostSettings:
    """The operator-supplied names a profile needs; all validated."""

    pg_data: str = ""
    root: str = ""
    health_addr: str = DEFAULT_HEALTH_ADDR
    vm: str = ""
    guest_host: str = ""
    guest_user: str = ""
    python: str = ""
    harness_dir: str = ""  # where the harness lives *on the guest*; default: here


def validated(profile: ShippedProfile, host: HostSettings) -> HostSettings:
    """Checks the names a real run needs; raises :class:`ProfileError` naming what is missing."""
    missing = []
    if not host.pg_data:
        missing.append("--pg-data (the bundled PostgreSQL data directory)")
    if profile.guest_os == "linux" and not host.root:
        missing.append("--root (the site root `spot --root ROOT serve` runs with)")
    if profile.target_kind == "vm" and not host.vm:
        missing.append("--vm (the disposable VM's name)")
    if profile.ssh_guest and not (host.guest_host and host.guest_user):
        missing.append("--guest-host and --guest-user (the VM is reached over SSH)")
    if missing:
        raise ProfileError(f"profile {profile.name}: missing " + "; ".join(missing))
    if host.vm:
        safe_token(host.vm, "vm name")
    return host


def _python(profile: ShippedProfile, host: HostSettings) -> str:
    if host.python:
        return host.python
    return "python" if profile.guest_os == "windows" else "python3"


def _guest_path(host: HostSettings, *parts: str) -> str:
    base = host.harness_dir or os.path.dirname(HERE)
    return os.path.join(base, *parts)


def adapter_argv(profile: ShippedProfile, host: HostSettings, kind: str) -> tuple[str, ...]:
    """The argv list of one adapter command (``settings``, ``health``, ``pids``, ``snapshot``, ``extras``)."""
    python = _python(profile, host)
    script = _guest_path(host, "profiles", "shipped_adapters.py")
    if kind == "settings":
        return (python, script, "settings")
    if kind == "health":
        return (python, script, "health", "--addr", host.health_addr)
    if kind == "pids":
        argv = [python, script, "pids", "--pg-data", host.pg_data]
        if profile.guest_os == "windows":
            argv += ["--service", SERVICE_NAME]
        else:
            argv += ["--root", host.root]
        return tuple(argv)
    if kind in ("snapshot", "extras"):
        return (python, script, kind)
    raise ProfileError(f"unknown adapter {kind!r}")


def workload_argv(profile: ShippedProfile, host: HostSettings, environ: Mapping[str, str]) -> tuple[str, ...]:
    """The workload command: the guest agent delegating to the operator's streaming client."""
    client = environ.get(CLIENT_CMD_ENV, "").strip()
    if not client:
        raise ProfileError(
            f"{CLIENT_CMD_ENV} is not set: the profile needs a client that posts through the "
            "shipped host and streams the spot-l7-history v1 file"
        )
    return (_python(profile, host), _guest_path(host, "guest", "run_workload.py"), "--client-cmd", client)


def start_host_argv(profile: ShippedProfile, host: HostSettings) -> tuple[str, ...]:
    """How the shipped host is started on the guest (documented; the operator's unit does this at boot)."""
    if profile.guest_os == "windows":
        return ("sc.exe", "start", SERVICE_NAME)
    return ("spot", "--root", host.root or "<root>", "serve")


def boot_argv(profile: ShippedProfile, host: HostSettings, environ: Mapping[str, str]) -> tuple[str, ...]:
    """The command that brings the host back after a cut.

    A VM profile restarts the VM (the host starts at boot: a service set to start
    automatically). A process-kill profile has no machine to restart, so the operator names the
    command that starts the shipped host again in ``SPOT_POWERLOSS_START_CMD`` (a unit start,
    or the ``spot --root ROOT serve`` launcher): this file never guesses it.
    """
    if profile.target_kind != "vm":
        command = environ.get(START_CMD_ENV, "").strip()
        if not command:
            raise ProfileError(f"{START_CMD_ENV} is not set: name the command that starts the shipped host after the kill")
        return tuple(shlex.split(command))
    vm = safe_token(host.vm or "<vm>", "vm name")
    if profile.hypervisor == "hyperv":
        return ("powershell.exe", "-NoProfile", "-NonInteractive", "-Command", f"Start-VM -Name '{vm}'")
    return ("virsh", "start", vm)


def python_for_here() -> str:
    return sys.executable or "python3"
