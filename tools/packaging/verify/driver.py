"""The driver: one systemd container per distro, the scenario table inside it.

It builds nothing the package build produced: ``--package-dir`` holds what
``tools/packaging/build.py`` wrote, and this module mounts it into a throwaway systemd
container (``podman run --systemd=always``) and runs the scenarios with ``podman exec``. The
only images it builds are the harness images under ``harness/packaging-linux/<distro>/``,
whose sole job is to be a booted distribution with the package tooling present.

Everything machine-touching goes through the injected :class:`~runtime.Runner` and
:class:`~runtime.Clock`, so the whole flow is testable without podman, root or a network.
"""

from __future__ import annotations

import shutil
import tempfile
from pathlib import Path

import packages
import report
from context import PKG_MOUNT, WORK_MOUNT, Context, Scenario, ScenarioFailure, ScenarioSkip
from distros import SYSTEMD_POLL_S, SYSTEMD_READY_S, Distro
from runtime import Clock, CommandResult, Runner, SystemClock, tail

#: Where the harness image is built from: the directory holding its Containerfile.
BUILD_TIMEOUT = 3600
#: How many output lines of a setup failure the report keeps.
SETUP_LOG_LINES = 40


def image_build_argv(runtime: str, distro: Distro) -> list[str]:
    """The argv that builds the distro's systemd test image."""
    context_dir = distro.containerfile.parent
    return [
        runtime,
        "build",
        "--format",
        "oci",
        "-t",
        distro.image_tag,
        "-f",
        str(distro.containerfile),
        str(context_dir),
    ]


def container_create_argv(
    runtime: str, distro: Distro, package_dir: Path | None, work_dir: Path | None
) -> list[str]:
    """The argv that starts the distro container with systemd as PID 1.

    ``--systemd=always`` is what makes ``systemctl`` inside the container talk to a real
    systemd (the same choice ``harness/agent-linux`` makes). ``label=disable`` keeps a
    rootless run working on an SELinux host; each mount is one argv element, so a directory
    name with a space stays a directory name.
    """
    argv = [
        runtime,
        "run",
        "-d",
        "--name",
        distro.container_name,
        "--systemd=always",
        "--security-opt",
        "label=disable",
    ]
    if package_dir is not None:
        argv += ["-v", f"{package_dir}:{PKG_MOUNT}:ro,z"]
    if work_dir is not None:
        argv += ["-v", f"{work_dir}:{WORK_MOUNT}:rw,z"]
    argv += [distro.image_tag]
    return argv


def container_remove_argv(runtime: str, distro: Distro) -> list[str]:
    """The argv that force-removes the distro's container."""
    return [runtime, "rm", "-f", distro.container_name]


def systemd_ready_argv(runtime: str, distro: Distro) -> list[str]:
    """The argv that asks systemd inside the container whether it has finished booting."""
    return [runtime, "exec", distro.container_name, "systemctl", "is-system-running"]


def cgroup_v2_present(runner: Runner) -> bool:
    """Whether the host exposes a cgroup v2 hierarchy, which rootless podman needs here."""
    return runner.exists("/sys/fs/cgroup/cgroup.controllers")


class Driver:
    """Runs the scenario table across the selected distros."""

    def __init__(
        self,
        runner: Runner,
        clock: Clock,
        sleep,
        *,
        runtime: str = "podman",
        package_dir: Path | None = None,
        binary: Path | None = None,
        keep: bool = False,
        emit=print,
    ) -> None:
        self.runner = runner
        self.clock = clock
        self.sleep = sleep
        self.runtime = runtime
        self.package_dir = package_dir
        self.binary = binary
        self.keep = keep
        self.emit = emit
        self.artefacts = packages.discover(package_dir)
        self.work_dir = Path(tempfile.mkdtemp(prefix="spot-pkgverify-"))

    # -- lifecycle ----------------------------------------------------------

    def close(self) -> None:
        """Delete the fixture directory unless ``--keep`` asked to keep it."""
        if self.keep:
            self.emit(f"kept the work directory: {self.work_dir}")
            return
        shutil.rmtree(self.work_dir, ignore_errors=True)

    def context(self, distro: Distro) -> Context:
        """The context a scenario for ``distro`` runs with."""
        return Context(
            distro=distro,
            runner=self.runner,
            clock=self.clock,
            sleep=self.sleep,
            runtime=self.runtime,
            container=distro.container_name,
            artefacts=self.artefacts,
            package_dir=self.package_dir,
            binary=self.binary,
            work_dir=self.work_dir,
        )

    # -- running ------------------------------------------------------------

    def run(
        self, distro_order: tuple[Distro, ...], selected: tuple[Scenario, ...]
    ) -> list[report.ScenarioResult]:
        """Run every selected scenario on every selected distro, then the shared ones."""
        rows: list[report.ScenarioResult] = []
        for distro in distro_order:
            rows.extend(self.run_distro(distro, selected))
        chosen_shared = tuple(scenario for scenario in selected if scenario.shared)
        for scenario in chosen_shared:
            rows.append(self.run_shared(scenario, distro_order[0]))
        return rows

    def run_distro(
        self, distro: Distro, selected: tuple[Scenario, ...]
    ) -> list[report.ScenarioResult]:
        """Start one container and run every non-shared selected scenario in it."""
        chosen = tuple(scenario for scenario in selected if not scenario.shared)
        if not chosen:
            return []
        self.emit(f"{distro.name}: building {distro.image_tag}")
        setup_error, log = self.prepare(distro)
        rows: list[report.ScenarioResult] = []
        statuses: dict[str, str] = {}
        context = self.context(distro)
        try:
            for scenario in chosen:
                blocked = [name for name in scenario.requires if statuses.get(name) != report.PASS]
                if blocked:
                    rows.append(
                        report.make(
                            distro.name,
                            scenario.name,
                            report.SKIP,
                            0,
                            "needs " + ", ".join(blocked),
                        )
                    )
                    statuses[scenario.name] = report.SKIP
                    continue
                if setup_error:
                    rows.append(
                        report.make(
                            distro.name,
                            scenario.name,
                            report.ERROR,
                            0,
                            setup_error,
                            log,
                        )
                    )
                    statuses[scenario.name] = report.ERROR
                    continue
                row = self._execute(context, distro.name, scenario)
                rows.append(row)
                statuses[scenario.name] = row.status
        finally:
            if not setup_error:
                self._remove_container(distro)
        return rows

    def run_shared(self, scenario: Scenario, distro: Distro) -> report.ScenarioResult:
        """Run one shared scenario once, on the host, against the first selected distro."""
        context = self.context(distro)
        return self._execute(context, report.SHARED_DISTRO, scenario)

    def _execute(
        self, context: Context, distro_name: str, scenario: Scenario
    ) -> report.ScenarioResult:
        """Run one scenario and turn its outcome into a report row."""
        self.emit(f"{distro_name}: {scenario.name}")
        started = self.clock.now_ms()
        status = report.PASS
        detail = ""
        log: list[str] = []
        try:
            detail = scenario.run(context)
        except ScenarioSkip as skip:
            status = report.SKIP
            detail = str(skip)
        except ScenarioFailure as failure:
            status = report.FAIL
            detail = str(failure)
            log = list(context.log)
        except Exception as error:  # noqa: BLE001 - a crash is an error row, not a traceback
            status = report.ERROR
            detail = f"{type(error).__name__}: {error}"
            log = list(context.log)
        duration_ms = self.clock.now_ms() - started
        row = report.make(distro_name, scenario.name, status, duration_ms, detail, log)
        self.emit(f"{distro_name}: {scenario.name}: {status}" + (f" ({detail})" if detail else ""))
        return row

    # -- the container ------------------------------------------------------

    def prepare(self, distro: Distro) -> tuple[str, list[str]]:
        """Build the image, start the container and wait for systemd.

        Returns ``("", [])`` on success, or an error message with the log lines to keep.
        """
        build = self.runner.run(image_build_argv(self.runtime, distro), timeout=BUILD_TIMEOUT)
        if not build.ok:
            return f"could not build {distro.image_tag}", _lines(build)

        create = self.runner.run(
            container_create_argv(self.runtime, distro, self.package_dir, self.work_dir),
            timeout=BUILD_TIMEOUT,
        )
        if not create.ok:
            return f"could not start the {distro.name} container", _lines(create)

        return self.wait_for_systemd(distro)

    def wait_for_systemd(self, distro: Distro) -> tuple[str, list[str]]:
        """Poll ``is-system-running`` until it answers ``running``/``degraded``.

        Returns ``("", [])`` when systemd is ready, or the error with the last answers.
        """
        start = self.clock.now_ms()
        answers: list[str] = []
        while True:
            result = self.runner.run(systemd_ready_argv(self.runtime, distro), timeout=60)
            answer = result.stdout.strip() or result.stderr.strip()
            answers.append(f"is-system-running: {answer or '(no output)'}")
            if answer in ("running", "degraded"):
                return "", []
            if self.clock.now_ms() - start >= SYSTEMD_READY_S * 1000:
                return (
                    f"systemd did not become ready within {SYSTEMD_READY_S} s "
                    f"(is-system-running said {answer!r})",
                    list(tail(answers, SETUP_LOG_LINES)),
                )
            self.sleep(SYSTEMD_POLL_S)

    def _remove_container(self, distro: Distro) -> None:
        """Remove the container, or say that it was kept."""
        if self.keep:
            self.emit(f"kept the container: {distro.container_name}")
            return
        self.runner.run(container_remove_argv(self.runtime, distro), timeout=120)


def _lines(result: CommandResult) -> list[str]:
    """The last output lines of a failed setup command, for the report."""
    return list(tail(result.output().splitlines(), SETUP_LOG_LINES))


def default_clock() -> Clock:
    """The clock the command line uses: the system's monotonic one."""
    return SystemClock()
