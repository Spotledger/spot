"""What a scenario gets to work with, and how it fails.

A :class:`Context` is the only way a scenario touches the machine: :meth:`Context.exec`
runs one command inside the systemd container, :meth:`Context.host` runs one on the host
(the container image scenario needs that), and :meth:`Context.poll_until` waits on a probe
using the injected clock and sleep. A failed assertion raises :class:`ScenarioFailure` with
a message; a missing prerequisite raises :class:`ScenarioSkip` with a reason. Neither is a
pass, and the driver records which one happened.
"""

from __future__ import annotations

from dataclasses import dataclass, field
from pathlib import Path
from typing import Callable, Protocol

import commands
import packages
from distros import Distro
from runtime import Clock, CommandResult, Runner


class ScenarioFailure(Exception):
    """A scenario's assertion did not hold; the message is what the report shows."""


class ScenarioSkip(Exception):
    """A scenario cannot run here (no artefact, no release binary); carries the reason."""


@dataclass(frozen=True)
class Step:
    """One command a scenario will run, with what its result has to be."""

    label: str
    argv: tuple[str, ...]
    expect: str


class Probe(Protocol):
    """A condition polled until it holds or the deadline passes."""

    def __call__(self) -> bool:
        ...


@dataclass
class Context:
    """Everything one scenario may use: the distro, the container, the clock, the artefacts."""

    distro: Distro
    runner: Runner
    clock: Clock
    sleep: Callable[[int], None]
    runtime: str
    container: str
    artefacts: tuple[packages.Artefact, ...] = ()
    package_dir: Path | None = None
    binary: Path | None = None
    work_dir: Path | None = None
    log: list[str] = field(default_factory=list)

    # -- running commands ---------------------------------------------------

    def exec(self, argv: list[str], *, timeout: int | None = None) -> CommandResult:
        """Run ``argv`` inside the container through ``podman exec``."""
        wrapped = [self.runtime, "exec", self.container, *argv]
        return self._run(wrapped, argv, timeout=timeout)

    def host(self, argv: list[str], *, timeout: int | None = None) -> CommandResult:
        """Run ``argv`` on the host, for the container-image scenario."""
        return self._run(argv, argv, timeout=timeout)

    def _run(
        self, wrapped: list[str], inner: list[str], *, timeout: int | None
    ) -> CommandResult:
        result = self.runner.run(wrapped, timeout=timeout)
        self.note("$ " + " ".join(inner))
        self.note(f"  exit {result.returncode}")
        for line in result.output().splitlines()[:20]:
            self.note("  | " + line)
        return result

    def note(self, text: str) -> None:
        """Add one line to the scenario log (the report keeps the last 40 on failure)."""
        self.log.append(text)

    def check(self, condition: bool, message: str) -> None:
        """Fail the scenario unless ``condition`` holds."""
        if not condition:
            raise ScenarioFailure(message)

    def check_ok(self, result: CommandResult, message: str) -> None:
        """Fail the scenario unless the command exited zero."""
        if not result.ok:
            detail = result.first_line() or "no output"
            raise ScenarioFailure(f"{message} (exit {result.returncode}: {detail})")

    # -- waiting ------------------------------------------------------------

    def poll_until(
        self, probe: Probe, *, deadline_s: int, interval_s: int, what: str
    ) -> int:
        """Poll ``probe`` until it holds; return the elapsed whole seconds.

        Raises:
            ScenarioFailure: when the probe still does not hold at the deadline.
        """
        start = self.clock.now_ms()
        deadline_ms = deadline_s * 1000
        while True:
            if probe():
                return (self.clock.now_ms() - start) // 1000
            if self.clock.now_ms() - start >= deadline_ms:
                raise ScenarioFailure(f"{what} did not happen within {deadline_s} s")
            self.sleep(interval_s)

    # -- artefacts ----------------------------------------------------------

    def package(self) -> packages.Artefact:
        """The newest artefact for this distro's package format.

        Raises:
            ScenarioSkip: when the build step produced no such package.
        """
        return self._pick(latest=True)

    def first_package(self) -> packages.Artefact:
        """The oldest artefact: the version a fresh install starts from, so that the
        upgrade scenario that runs after it has something to upgrade.

        Raises:
            ScenarioSkip: when the build step produced no such package.
        """
        return self._pick(latest=False)

    def _pick(self, *, latest: bool) -> packages.Artefact:
        matching = packages.for_format(self.artefacts, self.distro.package_format)
        artefact = matching[-1] if latest and matching else (matching[0] if matching else None)
        if artefact is None:
            raise ScenarioSkip(
                f"no .{self.distro.package_format} artefact in the package directory"
            )
        return artefact

    def pair(self) -> tuple[packages.Artefact, packages.Artefact]:
        """The oldest and newest artefact for this distro, to prove an upgrade.

        Raises:
            ScenarioSkip: when fewer than two versions are present.
        """
        found = packages.upgrade_pair(self.artefacts, self.distro.package_format)
        if found is None:
            raise ScenarioSkip(
                "needs two versions of the ."
                f"{self.distro.package_format} package, found "
                f"{len(packages.for_format(self.artefacts, self.distro.package_format))}"
            )
        return found

    def container_path(self, artefact: packages.Artefact) -> str:
        """Where the artefact is mounted inside the container: one argv element."""
        return f"{PKG_MOUNT}/{artefact.filename}"

    def work_path(self, name: str) -> str:
        """Where a fixture written into the work directory appears inside the container."""
        return f"{WORK_MOUNT}/{name}"

    def work_file(self, name: str, content: str) -> str:
        """Write a fixture into the work directory and return its container path."""
        if self.work_dir is None:
            raise ScenarioFailure("the driver did not create a work directory")
        path = Path(self.work_dir) / name
        path.write_text(content, encoding="utf-8")
        return self.work_path(name)

    def work_copy(self, name: str, source: Path) -> str:
        """Copy a host file into the work directory and return its container path."""
        if self.work_dir is None:
            raise ScenarioFailure("the driver did not create a work directory")
        destination = Path(self.work_dir) / name
        destination.write_bytes(source.read_bytes())
        return self.work_path(name)

    # -- queries ------------------------------------------------------------

    def installed_version(self) -> str:
        """The installed version as the package manager reports it; empty when absent."""
        result = self.exec(commands.installed_version(self.distro))
        return result.stdout.strip() if result.ok else ""

    def unit_properties(self, *names: str) -> dict[str, str]:
        """``systemctl show`` for the named properties, as a mapping."""
        result = self.exec(commands.unit_show(*names))
        return commands.parse_show(result.stdout)


#: Where the driver mounts ``--package-dir`` inside the container (read only).
PKG_MOUNT = "/run/spot-pkgs"
#: Where the driver mounts its fixture directory inside the container (read/write).
WORK_MOUNT = "/run/spot-verify"


@dataclass(frozen=True)
class Scenario:
    """One named proof: what it demonstrates, how it is planned and how it is run."""

    name: str
    proves: str
    run: Callable[[Context], str]
    plan: Callable[..., tuple[Step, ...]]
    requires: tuple[str, ...] = ()
    shared: bool = False
