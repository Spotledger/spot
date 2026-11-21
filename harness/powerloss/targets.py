"""Targets: the machine the service runs on, and how the harness talks to it.

A *target* is one disposable machine (or VM) with the Spotledger service and PostgreSQL on
it. The harness only ever cuts a target it provisioned itself; :func:`assert_disposable`
is the guard, and every driver goes through it.

Three implementations:

- :class:`FakeTarget` — the model store, run in-process. Used by the tests and by
  ``--selftest`` when no real target is configured. It is fast, deterministic and honest
  about being a model (see ``fakestore.py``).
- :class:`LocalTarget` — a disposable PostgreSQL cluster on the local Linux host, plus
  operator-supplied workload and snapshot commands. This is the Linux equivalent that runs
  where a service build is available.
- :class:`VmTarget` — a VM reset through libvirt (``virsh``) or Hyper-V (``Stop-VM``), with
  the workload and snapshot commands run inside the guest over SSH.

Times are integer microseconds; the target never reads the wall clock for the *result*, it
measures with the clock it is given (``clock`` is an argument).
"""

from __future__ import annotations

import json
import os
import shlex
import subprocess
import time
from dataclasses import dataclass, field
from typing import Callable, Protocol, Sequence

from checks import CheckReport, check_all
from fakestore import FakeStore
from l7history import History, read_history
from storesnapshot import StoreSnapshot
from workload import PostingClient, RunStats, WorkloadConfig

DURABLE_ON = "on"


class TargetError(Exception):
    """The target could not do what was asked."""


class SettingsError(TargetError):
    """A PostgreSQL durability setting is off when it must be on."""


def assert_durable_settings(settings: dict[str, object]) -> None:
    """Fails unless ``fsync``, ``synchronous_commit`` and ``full_page_writes`` are all on.

    The product defaults are all on; a campaign that runs with any of them off is the
    deliberate negative control (which skips this assertion) or a misconfiguration that would
    make a green run meaningless.
    """
    fsync = str(settings.get("fsync", "")).lower()
    full_page = str(settings.get("full_page_writes", "")).lower()
    sync = str(settings.get("synchronous_commit", "")).lower()
    problems = []
    if fsync != DURABLE_ON:
        problems.append(f"fsync={fsync or 'missing'} (must be on)")
    if full_page != DURABLE_ON:
        problems.append(f"full_page_writes={full_page or 'missing'} (must be on)")
    if sync != DURABLE_ON:
        problems.append(f"synchronous_commit={sync or 'missing'} (must be on)")
    if problems:
        raise SettingsError("durability settings are off: " + "; ".join(problems))


@dataclass(frozen=True)
class RecoveryInfo:
    """How long recovery took after a cut."""

    recovery_us: int
    wal_replay_us: int

    def to_json(self) -> dict[str, int]:
        return {"recovery_us": self.recovery_us, "wal_replay_us": self.wal_replay_us}


class CycleTarget(Protocol):
    """The target surface the campaign uses."""

    name: str

    def is_disposable(self) -> bool: ...
    def settings(self) -> dict[str, object]: ...
    def prepare(self, seed: int) -> None: ...
    def start_workload(self, seed: int, cut_after_ops: int) -> None: ...
    def workload_progress(self) -> int: ...
    def clock(self) -> int: ...
    def cut(self, driver_name: str) -> list[str]: ...
    def boot(self) -> None: ...
    def wait_recovery(self, timeout_us: int) -> RecoveryInfo: ...
    def history(self) -> History: ...
    def snapshot(self) -> StoreSnapshot: ...


# -- the model target -----------------------------------------------------------------------


@dataclass
class FakeTarget:
    """The model store as a target. Deterministic and in-process."""

    durability: dict[str, bool] = field(default_factory=lambda: {"fsync": True, "synchronous_commit": True, "full_page_writes": True})
    config: WorkloadConfig = field(default_factory=WorkloadConfig)
    history_path: str = ""
    clock: Callable[[], int] = lambda: 0
    recovery_us: int = 0
    wal_replay_us: int = 0
    name: str = "fake"
    vm_name: str = "fake-vm"
    device: str = "/dev/fake"
    _store: FakeStore | None = field(default=None, init=False)
    _history: History | None = field(default=None, init=False)
    _stats: RunStats | None = field(default=None, init=False)
    _commands: list[list[str]] = field(default_factory=list, init=False)

    def is_disposable(self) -> bool:
        return True

    def settings(self) -> dict[str, object]:
        if self._store is None:
            return {}
        return self._store.settings()

    def prepare(self, seed: int) -> None:
        self._store = FakeStore(**self.durability)
        self.config = WorkloadConfig(
            seed=seed, workers=self.config.workers, operations=self.config.operations, profile=self.config.profile
        )
        self._history = None
        self._stats = None
        self._commands = []

    def start_workload(self, seed: int, cut_after_ops: int) -> None:
        if self._store is None:
            raise TargetError("prepare() first")
        client = PostingClient(
            self._store,
            self.config,
            self.history_path,
            self.clock,
            stop_after=cut_after_ops,
        )
        self._stats = client.run()

    def workload_progress(self) -> int:
        return 0 if self._stats is None else self._stats.operations

    def cut(self, driver_name: str) -> list[str]:
        if self._store is None:
            raise TargetError("prepare() first")
        from cutdrivers import driver_by_name, perform_cut

        driver = driver_by_name(driver_name)()
        if not self.is_disposable():
            raise TargetError("refusing to cut a non-disposable target")
        plan = driver.plan(self)
        for argv in plan:
            self._commands.append(list(argv))
        self._store.cut()
        return plan

    def boot(self) -> None:
        # A model cut needs no boot; recovery is the WAL replay the store did in cut().
        return

    def wait_recovery(self, timeout_us: int) -> RecoveryInfo:
        return RecoveryInfo(recovery_us=self.recovery_us, wal_replay_us=self.wal_replay_us)

    def history(self) -> History:
        if self._history is None:
            self._history = read_history(self.history_path)
        return self._history

    def snapshot(self) -> StoreSnapshot:
        if self._store is None:
            raise TargetError("prepare() first")
        return self._store.snapshot()

    # -- helpers for tests ------------------------------------------------------------------

    def service_pids(self) -> list[int]:
        return [4242]

    def execute(self, argv: Sequence[str], *, timeout: float = 60.0) -> str:
        self._commands.append(list(argv))
        return ""

    def simulate_cut(self, driver_name: str) -> None:
        self.cut(driver_name)

    def commands(self) -> list[list[str]]:
        return list(self._commands)


# -- real targets ---------------------------------------------------------------------------


@dataclass
class CommandTarget:
    """Base for real targets: command plumbing, recovery waiting and the disposable guard.

    ``runner`` runs an argv list **locally** (the harness host); ``guest_runner`` runs an argv
    list **inside the target** (over SSH or a guest agent). Both are argv lists; nothing is
    shelled. ``clock`` supplies microseconds.
    """

    name: str
    runner: Callable[[Sequence[str], float], str]
    guest_runner: Callable[[Sequence[str], float], str]
    clock: Callable[[], int]
    history_path: str
    workload_argv: tuple[str, ...] = ()
    snapshot_argv: tuple[str, ...] = ()
    settings_argv: tuple[str, ...] = ()
    health_argv: tuple[str, ...] = ()
    pids_argv: tuple[str, ...] = ()
    boot_argv: tuple[str, ...] = ()
    disposable: bool = True
    vm_name: str = ""
    device: str = ""
    spawn: Callable[[Sequence[str]], "subprocess.Popen[str]"] | None = None
    _commands: list[list[str]] = field(default_factory=list, init=False)
    _workload_proc: "subprocess.Popen[str] | None" = field(default=None, init=False)

    def _spawn(self, argv: Sequence[str]) -> "subprocess.Popen[str]":
        if self.spawn is not None:
            return self.spawn(argv)
        try:
            return subprocess.Popen(list(argv), stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        except OSError as exc:
            raise TargetError(f"cannot start workload {' '.join(argv)}: {exc}") from exc

    def is_disposable(self) -> bool:
        return self.disposable

    def _run(self, argv: Sequence[str], timeout: float, *, guest: bool) -> str:
        self._commands.append(list(argv))
        runner = self.guest_runner if guest else self.runner
        return runner(argv, timeout)

    def execute(self, argv: Sequence[str], *, timeout: float = 60.0) -> str:
        return self._run(argv, timeout, guest=False)

    def settings(self) -> dict[str, object]:
        if not self.settings_argv:
            raise TargetError("no settings command configured; cannot assert durability")
        text = self._run(self.settings_argv, 30.0, guest=True)
        try:
            return json.loads(text)
        except json.JSONDecodeError as exc:
            raise TargetError(f"settings command did not return JSON: {exc}") from exc

    def prepare(self, seed: int) -> None:
        self._commands = []
        # A fresh history each cycle; the workload command is expected to truncate it.
        try:
            os.remove(self.history_path)
        except FileNotFoundError:
            pass

    def start_workload(self, seed: int, cut_after_ops: int) -> None:
        if not self.workload_argv:
            raise TargetError("no workload command configured")
        argv = list(self.workload_argv) + ["--seed", str(seed), "--history", self.history_path, "--cut-after", str(cut_after_ops)]
        # The workload runs in the background; the campaign cuts while it is in flight.
        self._workload_proc = self._spawn(argv)

    def workload_progress(self) -> int:
        if not self.history_path or not os.path.exists(self.history_path):
            return 0
        try:
            with open(self.history_path, "r", encoding="utf-8") as handle:
                return max(0, sum(1 for _ in handle) - 1)
        except OSError:
            return 0

    def service_pids(self) -> list[int]:
        if not self.pids_argv:
            return []
        text = self._run(self.pids_argv, 30.0, guest=True)
        return [int(token) for token in text.split() if token.strip().isdigit()]

    def cut(self, driver_name: str) -> list[str]:
        from cutdrivers import driver_by_name

        driver = driver_by_name(driver_name)()
        from cutdrivers import perform_cut

        return perform_cut(driver, self, "windows" if os.name == "nt" else "linux")

    def boot(self) -> None:
        if self.boot_argv:
            self._run(self.boot_argv, 300.0, guest=False)

    def wait_recovery(self, timeout_us: int) -> RecoveryInfo:
        if not self.health_argv:
            raise TargetError("no health command configured; cannot wait for recovery")
        started = self.clock()
        while True:
            try:
                self._run(self.health_argv, 30.0, guest=True)
                return RecoveryInfo(recovery_us=self.clock() - started, wal_replay_us=0)
            except TargetError:
                if self.clock() - started >= timeout_us:
                    raise TargetError(f"service did not recover within {timeout_us} us")
                time.sleep(1.0)

    def history(self) -> History:
        return read_history(self.history_path)

    def snapshot(self) -> StoreSnapshot:
        if not self.snapshot_argv:
            raise TargetError("no snapshot command configured; cannot read the store")
        text = self._run(self.snapshot_argv, 60.0, guest=True)
        return StoreSnapshot.from_json_text(text)


def subprocess_runner(argv: Sequence[str], timeout: float) -> str:
    """Runs an argv list with no shell; raises :class:`TargetError` on failure."""
    try:
        completed = subprocess.run(
            list(argv), capture_output=True, text=True, timeout=timeout, check=False
        )
    except (OSError, subprocess.TimeoutExpired) as exc:
        raise TargetError(f"{' '.join(argv)}: {exc}") from exc
    if completed.returncode != 0:
        raise TargetError(
            f"{' '.join(argv)} exited {completed.returncode}: {completed.stderr.strip()[:400]}"
        )
    return completed.stdout


def ssh_runner(host: str, user: str) -> Callable[[Sequence[str], float], str]:
    """A guest runner that runs argv lists over SSH (the command is one quoted string)."""

    def run(argv: Sequence[str], timeout: float) -> str:
        remote = " ".join(shlex.quote(token) for token in argv)
        return subprocess_runner(
            ["ssh", "-o", "BatchMode=yes", "-o", "ConnectTimeout=10", f"{user}@{host}", remote],
            timeout,
        )

    return run


def run_cycle(
    target: CycleTarget,
    driver_name: str,
    *,
    seed: int,
    cut_after_ops: int,
    recovery_timeout_us: int,
    enforce_settings: bool = True,
    os_name: str = "linux",
    wait_timeout_us: int = 300_000_000,
) -> tuple[CheckReport, RecoveryInfo, dict[str, object]]:
    """One cut cycle: prepare, assert settings, load, cut, boot, recover, check."""
    from cutdrivers import CutError, driver_by_name

    target.prepare(seed)
    settings = target.settings()
    if enforce_settings:
        assert_durable_settings(settings)
    target.start_workload(seed, cut_after_ops)
    # Wait until the workload has acknowledged the cut point, then cut it in flight.
    deadline = target.clock() + wait_timeout_us
    while target.workload_progress() < cut_after_ops:
        if target.clock() >= deadline:
            raise TargetError(
                f"workload did not reach the cut point ({cut_after_ops} operations) in time"
            )
        time.sleep(0.05)
    driver = driver_by_name(driver_name)()
    if not driver.supported(os_name):
        raise CutError(f"driver {driver_name!r} does not support {os_name}")
    target.cut(driver_name)
    target.boot()
    recovery = target.wait_recovery(recovery_timeout_us)
    history = target.history()
    snapshot = target.snapshot()
    report = check_all(history, snapshot, seed=seed)
    return report, recovery, settings
