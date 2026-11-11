"""The process and clock seam, so the scenario logic is testable without a host.

Everything that touches the machine — running ``podman``, reading a file — goes through a
:class:`Runner`; everything that needs time goes through a :class:`Clock` and a ``sleep``
callable. The real implementations are :class:`SubprocessRunner` and
:class:`SystemClock`; the tests drive :class:`StubRunner` and :class:`FixedClock` and assert
on the exact argv that was built.

Time is an argument, never read from the wall clock inside the scenarios: the driver passes
the clock down, and every duration in the report is a whole number of milliseconds derived
from ``monotonic_ns``. Standard library only.
"""

from __future__ import annotations

import shutil
import subprocess
import time
from dataclasses import dataclass, field
from pathlib import Path
from typing import Protocol, runtime_checkable

#: The default timeout for one command, in seconds. A container install is slow.
DEFAULT_TIMEOUT = 1800

#: How many log lines a failed scenario keeps in the report.
LOG_LINES = 40


@dataclass(frozen=True)
class CommandResult:
    """The outcome of one command: the argv, the exit code and the captured output."""

    argv: tuple[str, ...]
    returncode: int
    stdout: str = ""
    stderr: str = ""

    @property
    def ok(self) -> bool:
        """Whether the command exited zero."""
        return self.returncode == 0

    def output(self) -> str:
        """Standard output and standard error, in that order."""
        return (self.stdout or "") + (self.stderr or "")

    def first_line(self) -> str:
        """The first non-empty line of output, stripped; empty when there is none."""
        for line in self.output().splitlines():
            stripped = line.strip()
            if stripped:
                return stripped
        return ""


@runtime_checkable
class Runner(Protocol):
    """Runs commands, so the scenarios never call the operating system directly."""

    def run(
        self,
        argv: list[str] | tuple[str, ...],
        *,
        timeout: int | None = None,
        cwd: str | None = None,
    ) -> CommandResult:
        """Run ``argv`` and return its result; never raises on a non-zero exit."""
        ...

    def which(self, program: str) -> str | None:
        """The path of ``program`` on ``PATH``, or ``None`` when it is not installed."""
        ...

    def exists(self, path: str) -> bool:
        """Whether ``path`` exists."""
        ...


class SubprocessRunner:
    """The real runner over :mod:`subprocess` and :mod:`shutil`."""

    def run(
        self,
        argv: list[str] | tuple[str, ...],
        *,
        timeout: int | None = None,
        cwd: str | None = None,
    ) -> CommandResult:
        argv = tuple(str(item) for item in argv)
        try:
            completed = subprocess.run(  # noqa: S603 - argv is a list built here, never a shell string
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


class UnscriptedCommand(AssertionError):
    """A test ran a command the stub was not told about.

    Raised instead of silently succeeding, so a test that forgets a response fails loudly
    rather than proving a scenario against invented output.
    """


def contains_argv(argv: tuple[str, ...], needle: tuple[str, ...]) -> bool:
    """Whether ``needle`` appears in ``argv`` as a contiguous run of elements."""
    if not needle:
        return False
    width = len(needle)
    for start in range(len(argv) - width + 1):
        if argv[start:start + width] == needle:
            return True
    return False


@dataclass
class StubRunner:
    """A runner for tests: scripted results, recorded calls, no process is ever started.

    :meth:`add` scripts one response; the match is a contiguous run of argv elements, so a
    script can ignore the ``podman exec`` wrapper the context adds. A response is consumed
    the first time it matches, so a test scripts the exact sequence — the same query before
    and after an upgrade gets two entries with two answers. A command the test did not
    script raises :class:`UnscriptedCommand`.
    """

    responses: list[tuple[tuple[str, ...], CommandResult, bool]] = field(
        default_factory=list
    )
    programs: dict[str, str | None] = field(default_factory=dict)
    existing: set[str] = field(default_factory=set)
    calls: list[tuple[str, ...]] = field(default_factory=list)
    timeouts: list[int | None] = field(default_factory=list)

    def add(
        self,
        match: tuple[str, ...] | list[str],
        *,
        returncode: int = 0,
        stdout: str = "",
        stderr: str = "",
        sticky: bool = False,
    ) -> StubRunner:
        """Script ``match`` to return ``returncode`` with the given output.

        A response is consumed by the first matching call; ``sticky=True`` keeps answering,
        which is what a poll that must never succeed needs.
        """
        self.responses.append(
            (tuple(match), CommandResult(tuple(match), returncode, stdout, stderr), sticky)
        )
        return self

    def run(
        self,
        argv: list[str] | tuple[str, ...],
        *,
        timeout: int | None = None,
        cwd: str | None = None,
    ) -> CommandResult:
        argv = tuple(str(item) for item in argv)
        self.calls.append(argv)
        self.timeouts.append(timeout)
        for index, (match, result, sticky) in enumerate(self.responses):
            if contains_argv(argv, match):
                if not sticky:
                    del self.responses[index]
                return CommandResult(argv, result.returncode, result.stdout, result.stderr)
        raise UnscriptedCommand("no scripted response for: " + " ".join(argv))

    def which(self, program: str) -> str | None:
        if program in self.programs:
            return self.programs[program]
        return f"/usr/bin/{program}"

    def exists(self, path: str) -> bool:
        return path in self.existing


@runtime_checkable
class Clock(Protocol):
    """A monotonic millisecond clock, injected so tests are deterministic."""

    def now_ms(self) -> int:
        """Milliseconds from an arbitrary fixed origin; never decreases."""
        ...


class SystemClock:
    """The real clock: ``monotonic_ns`` truncated to whole milliseconds."""

    def now_ms(self) -> int:
        return time.monotonic_ns() // 1_000_000


@dataclass
class FixedClock:
    """A clock for tests: returns the scripted ticks, then repeats the last one."""

    ticks: list[int] = field(default_factory=lambda: [0, 0])

    def now_ms(self) -> int:
        if len(self.ticks) > 1:
            return self.ticks.pop(0)
        return self.ticks[0] if self.ticks else 0


def tail(lines: list[str], limit: int = LOG_LINES) -> tuple[str, ...]:
    """The last ``limit`` lines, which is what the report keeps for a failure."""
    if limit <= 0:
        return ()
    return tuple(lines[-limit:])
