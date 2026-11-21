"""The cut drivers: how the harness approximates a power cut.

Every driver is an **argv list**, never a shell string, so nothing is re-parsed by a shell.
Where a driver must embed a name (a VM or a device) it validates the token first. A driver
that cannot work on the current host says so through :meth:`CutDriver.supported`; the
orchestrator refuses to run it rather than fall back silently.

Three drivers, weakest to strongest:

- :class:`ProcessKill` — kill the service and PostgreSQL process tree (``taskkill /F /T`` on
  Windows, ``kill -9``/``pkill -9`` on POSIX). Runs anywhere, but the OS page cache survives,
  so it cannot lose data the way a power cut does. The weakest.
- :class:`VmReset` — a hypervisor hard reset: ``virsh destroy`` (libvirt), ``virsh
  qemu-monitor-command ... system_reset`` (QEMU), or ``Stop-VM -TurnOff`` (Hyper-V). This is
  the real approximation.
- :class:`DiskFreeze` — Linux device-mapper ``dm-flakey``/``dm-log-writes``: drop writes to a
  device for a window, for deterministic torn-write simulation. Optional.

The orchestrator only ever calls a driver through :func:`perform_cut`, which first proves the
target is **disposable** (see ``targets.assert_disposable``): a cut is never run against a
machine the harness did not provision itself.
"""

from __future__ import annotations

import re
from dataclasses import dataclass
from typing import Protocol, Sequence

_SAFE_TOKEN = re.compile(r"^[A-Za-z0-9._:/-]+$")


class CutError(Exception):
    """The cut could not be performed."""


def safe_token(value: str, what: str) -> str:
    """A name safe to embed in a command; refuses anything a shell could abuse."""
    if not value or not _SAFE_TOKEN.match(value):
        raise CutError(f"unsafe {what}: {value!r}")
    return value


class CutTarget(Protocol):
    """What a driver needs from a target."""

    def is_disposable(self) -> bool: ...

    def service_pids(self) -> list[int]: ...

    def execute(self, argv: Sequence[str], *, timeout: float = 60.0) -> str: ...

    def simulate_cut(self, driver_name: str) -> None: ...


@dataclass(frozen=True)
class CutDriver:
    """A named way to cut power."""

    name: str
    description: str
    platform: str  # "any" | "posix" | "windows" | "linux"

    def supported(self, os_name: str) -> bool:
        if self.platform == "any":
            return True
        if self.platform == "posix":
            return os_name != "windows"
        return self.platform == os_name

    def plan(self, target: CutTarget) -> list[list[str]]:
        """The argv lists the cut will run, in order. Pure: for the log and the tests."""
        raise NotImplementedError

    def perform(self, target: CutTarget) -> list[str]:
        """Runs the plan; returns the argv lists actually run."""
        plan = self.plan(target)
        for argv in plan:
            target.execute(argv)
        return plan


@dataclass(frozen=True)
class ProcessKill(CutDriver):
    """Kill the service and PostgreSQL process tree."""

    name: str = "process-kill"
    description: str = (
        "Kill the whole service + PostgreSQL process tree (taskkill /F /T on Windows, kill -9 "
        "on POSIX). Runs anywhere; the OS page cache survives, so this is the weakest cut."
    )
    platform: str = "any"
    pattern: str = ""

    def plan(self, target: CutTarget) -> list[list[str]]:
        pids = target.service_pids()
        if not pids:
            raise CutError("no service pids to kill; is the service running?")
        if self.pattern:
            safe_token(self.pattern, "kill pattern")
            return [["pkill", "-9", "-f", self.pattern]]
        return [["kill", "-9", str(pid)] for pid in pids]


@dataclass(frozen=True)
class ProcessKillWindows(CutDriver):
    """Kill the service tree on Windows with ``taskkill /F /T``."""

    name: str = "process-kill-windows"
    description: str = "taskkill /F /T on each service pid (Windows)."
    platform: str = "windows"

    def plan(self, target: CutTarget) -> list[list[str]]:
        pids = target.service_pids()
        if not pids:
            raise CutError("no service pids to kill; is the service running?")
        return [["taskkill", "/F", "/T", "/PID", str(pid)] for pid in pids]


@dataclass(frozen=True)
class VmReset(CutDriver):
    """A hypervisor hard reset of the target VM."""

    name: str = "vm-reset"
    description: str = (
        "Hard reset the target VM: virsh destroy (libvirt), the QEMU monitor system_reset, or "
        "Stop-VM -TurnOff (Hyper-V)."
    )
    platform: str = "any"
    hypervisor: str = "libvirt"  # libvirt | qemu | hyperv
    vm: str = ""

    def supported(self, os_name: str) -> bool:
        if self.hypervisor == "hyperv":
            return os_name == "windows"
        return os_name != "windows"

    def plan(self, target: CutTarget) -> list[list[str]]:
        vm = safe_token(self.vm or getattr(target, "vm_name", ""), "vm name")
        if self.hypervisor == "hyperv":
            command = f"Stop-VM -Name '{vm}' -TurnOff -Force"
            return [["powershell.exe", "-NoProfile", "-NonInteractive", "-Command", command]]
        if self.hypervisor == "qemu":
            return [["virsh", "qemu-monitor-command", vm, "--hmp", "system_reset"]]
        if self.hypervisor == "libvirt":
            return [["virsh", "destroy", vm]]
        raise CutError(f"unknown hypervisor {self.hypervisor!r}")


@dataclass(frozen=True)
class DiskFreeze(CutDriver):
    """Linux device-mapper drop-writes for deterministic torn-write simulation."""

    name: str = "disk-freeze"
    description: str = (
        "Linux dm-flakey: suspend and reload the mapping so writes are dropped for a window, "
        "then resume. Optional; needs root and a dedicated device."
    )
    platform: str = "linux"
    device: str = ""
    up_interval_ms: int = 0
    down_interval_ms: int = 1000
    freeze_ms: int = 0

    def table(self, device: str) -> str:
        """The ``dm-flakey`` table line for ``device``."""
        device = safe_token(device, "device")
        return (
            f"0 0 flakey {device} 0 {self.up_interval_ms} {self.down_interval_ms} "
            f"{1 if self.down_interval_ms else 0}"
        )

    def plan(self, target: CutTarget) -> list[list[str]]:
        device = safe_token(self.device or getattr(target, "device", ""), "device")
        name = safe_token(f"spot-powerloss-{self.name}", "mapping name")
        if self.freeze_ms:
            # Drop writes for the window: suspend, reload a table that errors writes, resume.
            return [
                ["dmsetup", "suspend", name],
                ["dmsetup", "reload", name, "--table", self.table(device)],
                ["dmsetup", "resume", name],
            ]
        return [["dmsetup", "create", name, "--table", self.table(device)]]


DRIVERS: dict[str, type[CutDriver]] = {
    ProcessKill.name: ProcessKill,
    ProcessKillWindows.name: ProcessKillWindows,
    VmReset.name: VmReset,
    DiskFreeze.name: DiskFreeze,
}


def driver_by_name(name: str) -> type[CutDriver]:
    """Looks up a driver class by its ``name``."""
    if name not in DRIVERS:
        known = ", ".join(sorted(DRIVERS))
        raise CutError(f"unknown cut driver {name!r}; known: {known}")
    return DRIVERS[name]


def perform_cut(driver: CutDriver, target: CutTarget, os_name: str) -> list[str]:
    """Guards the target is disposable, then performs the cut; returns the argv lists run."""
    if not target.is_disposable():
        raise CutError(
            "refusing to cut: the target is not marked disposable. A cut driver must never run "
            "against the host or any machine the harness did not provision."
        )
    if not driver.supported(os_name):
        raise CutError(f"driver {driver.name!r} does not support {os_name}")
    return driver.perform(target)
