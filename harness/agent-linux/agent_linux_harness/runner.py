"""The process and filesystem seam, so the orchestration logic is testable without a host.

Everything that touches the machine — running a container runtime, detecting a VM,
reading ``/etc/os-release`` — goes through a :class:`Runner`. The real one is
:class:`SubprocessRunner`; the tests drive a :class:`StubRunner` with scripted results and
assert on the exact argv the orchestrator built. Standard library only.
"""

from __future__ import annotations

import shutil
import subprocess
from dataclasses import dataclass, field
from pathlib import Path
from typing import Protocol, runtime_checkable

#: The default timeout for a container run, in seconds. A distro container that has to
#: start systemd and install a package is slow; the host-side commands are quick.
DEFAULT_TIMEOUT = 1800


@dataclass(frozen=True)
class CommandResult:
    """The outcome of one command."""

    argv: tuple[str, ...]
    returncode: int
    stdout: str = ""
    stderr: str = ""

    @property
    def ok(self) -> bool:
        """Whether the command exited zero."""
        return self.returncode == 0


@runtime_checkable
class Runner(Protocol):
    """Runs commands and reads files, so the orchestrator never calls the OS directly."""

    def run(
        self,
        argv: list[str] | tuple[str, ...],
        *,
        timeout: int | None = None,
        cwd: str | None = None,
    ) -> CommandResult:
        """Run ``argv`` in ``cwd`` (the current directory when ``None``) and return its result."""
        ...

    def which(self, program: str) -> str | None:
        """The absolute path of ``program`` on ``PATH``, or ``None``."""
        ...

    def exists(self, path: str) -> bool:
        """Whether ``path`` exists."""
        ...

    def read_text(self, path: str) -> str:
        """The contents of ``path``.

        Raises:
            OSError: when the file cannot be read.
        """
        ...


class SubprocessRunner:
    """The real runner over :mod:`subprocess`, :mod:`shutil` and :mod:`pathlib`."""

    def run(
        self,
        argv: list[str] | tuple[str, ...],
        *,
        timeout: int | None = None,
        cwd: str | None = None,
    ) -> CommandResult:
        argv = tuple(str(item) for item in argv)
        try:
            completed = subprocess.run(  # noqa: S603 - argv is built here, never a shell string
                argv,
                capture_output=True,
                text=True,
                timeout=timeout if timeout is not None else DEFAULT_TIMEOUT,
                check=False,
                cwd=cwd,
            )
        except FileNotFoundError:
            return CommandResult(argv, 127, "", f"{argv[0]}: command not found")
        except subprocess.TimeoutExpired as expired:
            out = expired.stdout or ""
            err = expired.stderr or ""
            if isinstance(out, bytes):
                out = out.decode("utf-8", "replace")
            if isinstance(err, bytes):
                err = err.decode("utf-8", "replace")
            return CommandResult(argv, 124, out, err)
        return CommandResult(argv, completed.returncode, completed.stdout, completed.stderr)

    def which(self, program: str) -> str | None:
        return shutil.which(program)

    def exists(self, path: str) -> bool:
        return Path(path).exists()

    def read_text(self, path: str) -> str:
        return Path(path).read_text(encoding="utf-8")


@dataclass
class StubRunner:
    """A runner for tests: scripted results, recorded calls, an in-memory file table.

    ``responses`` maps a command's ``argv[0]`` (or the full argv tuple) to a result; a
    command with no scripted response succeeds with empty output, which is what most
    orchestration tests want. Every call is recorded in :attr:`calls`.
    """

    responses: dict = field(default_factory=dict)
    programs: dict = field(default_factory=dict)
    files: dict = field(default_factory=dict)
    calls: list[tuple[str, ...]] = field(default_factory=list)
    cwds: list[str | None] = field(default_factory=list)

    def run(
        self,
        argv: list[str] | tuple[str, ...],
        *,
        timeout: int | None = None,
        cwd: str | None = None,
    ) -> CommandResult:
        argv = tuple(str(item) for item in argv)
        self.calls.append(argv)
        self.cwds.append(cwd)
        if argv in self.responses:
            return self.responses[argv]
        if argv and argv[0] in self.responses:
            return self.responses[argv[0]]
        return CommandResult(argv, 0, "", "")

    def which(self, program: str) -> str | None:
        if program in self.programs:
            return self.programs[program]
        return f"/usr/bin/{program}" if program in self.responses else None

    def exists(self, path: str) -> bool:
        return path in self.files

    def read_text(self, path: str) -> str:
        if path in self.files:
            return self.files[path]
        raise FileNotFoundError(path)
