"""The orchestration: decide what can run where, run it, and report the matrix.

The logic is split into two halves. The *planning* half (:func:`plan`,
:func:`plan_target`, :func:`container_argv`) is pure and is what the unit tests exercise:
given a matrix and a target, which tests run and which are skipped, and with what argv.
The *execution* half (:func:`execute`) drives a :class:`~agent_linux_harness.runner.Runner`
to pull or build an image, run each test in its container, and collect the results. It
never touches the host directly. Standard library only.
"""

from __future__ import annotations

import json
import os
from dataclasses import dataclass, field

from .junit import (
    ERROR,
    FAILED,
    PASSED,
    SKIPPED,
    TargetReport,
    TestResult,
    parse_libtest_output,
    run_duration,
)
from .matrix import Matrix, Target, TestCase
from .runner import CommandResult, Runner

#: Where the built test binary is mounted inside the container.
CONTAINER_BINARY = "/opt/spot/real-tests"

#: The cargo test target (the file ``tests/real.rs``) inside the real crate.
REAL_TEST_TARGET = "real"

#: The build timeout for ``cargo test --no-run`` (a cold workspace is slow).
BUILD_TIMEOUT = 3600


@dataclass(frozen=True)
class TestDecision:
    """Whether one test runs on one target, and why not when it does not."""

    test: TestCase
    run: bool
    reason: str = ""


@dataclass(frozen=True)
class TargetPlan:
    """What runs on one target."""

    target: Target
    decisions: tuple[TestDecision, ...]

    @property
    def runnable(self) -> tuple[TestDecision, ...]:
        """The decisions that will actually execute."""
        return tuple(decision for decision in self.decisions if decision.run)


@dataclass(frozen=True)
class RunPlan:
    """What runs across every selected target."""

    mode: str
    targets: tuple[TargetPlan, ...]

    @property
    def runnable_count(self) -> int:
        """How many test runs the plan will execute in total."""
        return sum(len(target.runnable) for target in self.targets)


def select_targets(
    matrix: Matrix, names: list[str] | None = None, mode: str = "container"
) -> tuple[Target, ...]:
    """The targets to run, filtered by ``mode`` and optionally by explicit ``names``.

    Raises:
        KeyError: when a requested name is not a target of the matrix.
    """
    selected = [target for target in matrix.targets if target.mode == mode]
    if names:
        wanted = set(names)
        known = {target.name for target in matrix.targets}
        unknown = sorted(wanted - known)
        if unknown:
            raise KeyError(f"unknown target(s): {unknown}")
        selected = [target for target in selected if target.name in wanted]
    return tuple(selected)


def plan_target(target: Target, tests: tuple[TestCase, ...], mode: str) -> TargetPlan:
    """Decide which of ``tests`` can run on ``target``.

    A test is skipped with ``needs vm`` when it is ``vm_only`` and the target is a
    container, and with ``needs <capability>`` when the target does not give one of the
    capabilities the test requires. Otherwise it runs.
    """
    decisions = []
    for test in tests:
        if test.vm_only and target.mode != "vm":
            decisions.append(TestDecision(test, False, "needs vm"))
            continue
        missing = sorted(test.requires - target.capabilities)
        if missing:
            decisions.append(TestDecision(test, False, "needs " + ", ".join(missing)))
            continue
        decisions.append(TestDecision(test, True))
    return TargetPlan(target, tuple(decisions))


def plan(
    matrix: Matrix, target_names: list[str] | None = None, mode: str = "container"
) -> RunPlan:
    """Plan every selected target.

    Raises:
        KeyError: when a requested target name is unknown.
    """
    targets = select_targets(matrix, target_names, mode)
    return RunPlan(
        mode=mode,
        targets=tuple(plan_target(target, matrix.tests, mode) for target in targets),
    )


def image_argv(runtime: str, target: Target, matrix_dir: str = ".") -> list[str]:
    """The argv that pulls or builds ``target``'s image."""
    if target.image_source == "pull":
        return [runtime, "pull", target.image]
    if target.image_source == "build":
        dockerfile = target.dockerfile or "Dockerfile"
        if not os.path.isabs(dockerfile):
            dockerfile = os.path.join(matrix_dir, dockerfile)
        return [runtime, "build", "-t", target.name, "-f", dockerfile, os.path.dirname(dockerfile)]
    raise ValueError(f"target {target.name} is not a container image (source {target.image_source})")


def container_name(target_name: str) -> str:
    """The deterministic name of the container for ``target_name``."""
    return f"spot-agent-linux-{target_name}"


def container_create_argv(
    runtime: str, target: Target, binary: str, name: str
) -> list[str]:
    """The argv that starts ``target``'s container, detached, and mounts the test binary.

    The container is started detached so a real systemd can be PID 1 and each test can be
    run with ``exec``: a systemd target needs ``--systemd=always`` (podman) or the cgroup
    and tmpfs plumbing (docker), and the test binary cannot be the entry point then. A
    non-systemd target keeps the test binary off the command line too — it runs a keep-alive
    shell — so every target follows the same create/exec/remove shape. A privileged target
    gets ``--privileged``; on an SELinux host podman needs ``--security-opt label=disable``
    and a ``z`` mount flag to read the mounted binary. The target's family and name are
    passed through so a test can assert which distribution it is running on.
    """
    argv = [runtime, "run", "-d", "--name", name]
    if target.gives("systemd"):
        if runtime == "podman":
            argv += ["--systemd=always"]
        else:
            argv += [
                "--cgroupns=host",
                "--tmpfs",
                "/run",
                "--tmpfs",
                "/run/lock",
                "-v",
                "/sys/fs/cgroup:/sys/fs/cgroup:rw",
                "--entrypoint",
                "/sbin/init",
            ]
    if target.gives("privileged"):
        argv += ["--privileged"]
    if runtime == "podman":
        argv += ["--security-opt", "label=disable"]
        mount = f"{binary}:{CONTAINER_BINARY}:ro,z"
    else:
        mount = f"{binary}:{CONTAINER_BINARY}:ro"
    argv += ["-v", mount]
    argv += [
        "-e",
        f"SPOT_TARGET_FAMILY={target.family}",
        "-e",
        f"SPOT_TARGET_NAME={target.name}",
    ]
    if not target.gives("systemd"):
        argv += ["--entrypoint", "/bin/sh"]
    argv += [target.image]
    if not target.gives("systemd"):
        argv += ["-c", "sleep infinity"]
    return argv


def container_exec_argv(runtime: str, name: str, test_name: str) -> list[str]:
    """The argv that runs one real test inside an already-running container."""
    return [
        runtime,
        "exec",
        name,
        CONTAINER_BINARY,
        "--ignored",
        "--exact",
        "--test-threads=1",
        test_name,
    ]


def container_remove_argv(runtime: str, name: str) -> list[str]:
    """The argv that force-removes the container."""
    return [runtime, "rm", "-f", name]


def wait_for_systemd(
    runner: Runner,
    runtime: str,
    name: str,
    timeout: int = 60,
    poll: float = 1.0,
    sleep=None,
) -> bool:
    """Wait until systemd inside the container reports ``running`` or ``degraded``.

    Returns whether it became ready within ``timeout`` seconds. A zero timeout does a
    single check, which is what the unit tests use.
    """
    import time as _time

    sleep = sleep or _time.sleep
    waited = 0.0
    while True:
        result = runner.run([runtime, "exec", name, "systemctl", "is-system-running"])
        if result.stdout.strip() in ("running", "degraded"):
            return True
        if waited >= timeout:
            return False
        sleep(poll)
        waited += poll


def parse_cargo_artifacts(lines: list[str], target_name: str = REAL_TEST_TARGET) -> list[str]:
    """The executable paths of cargo's JSON artifacts for the test target.

    ``cargo test --no-run --message-format=json`` emits one JSON object per line; a
    ``compiler-artifact`` with a non-null ``executable`` and a ``test`` target kind is a
    runnable test binary. Returns the matching executables in the order they appeared.
    """
    executables: list[str] = []
    for line in lines:
        line = line.strip()
        if not line.startswith("{"):
            continue
        try:
            message = json.loads(line)
        except json.JSONDecodeError:
            continue
        if message.get("reason") != "compiler-artifact":
            continue
        executable = message.get("executable")
        if not executable:
            continue
        target = message.get("target", {})
        kinds = target.get("kind", [])
        if "test" not in kinds:
            continue
        if target_name and target.get("name") != target_name:
            continue
        executables.append(executable)
    return executables


def build_test_binary(
    runner: Runner, crate_dir: str, target_name: str = REAL_TEST_TARGET
) -> str:
    """Build the real test crate and return its test binary path.

    Raises:
        RuntimeError: when cargo fails or produces no test binary.
    """
    result = runner.run(
        ["cargo", "test", "--no-run", "--message-format=json"],
        timeout=BUILD_TIMEOUT,
        cwd=crate_dir,
    )
    executables = parse_cargo_artifacts(result.stdout.splitlines(), target_name)
    if not executables:
        tail = (result.stderr or result.stdout)[-4000:]
        raise RuntimeError(f"cargo produced no test binary (exit {result.returncode}):\n{tail}")
    return executables[-1]


def detect_runtime(runner: Runner, preference: str = "auto") -> str:
    """The container runtime to use: podman preferred, docker accepted.

    Raises:
        RuntimeError: when the requested runtime (or, for ``auto``, either) is absent.
    """
    if preference != "auto":
        if runner.which(preference):
            return preference
        raise RuntimeError(f"the requested container runtime {preference!r} is not installed")
    for candidate in ("podman", "docker"):
        if runner.which(candidate):
            return candidate
    raise RuntimeError("neither podman nor docker is installed")


def detect_disposable_root(runner: Runner) -> tuple[bool, str]:
    """Whether the current root looks like a container or a VM, and what kind.

    Checks ``/.dockerenv`` and ``/run/.containerenv`` (the container runtimes' markers),
    then ``systemd-detect-virt``, then the init process's cgroup. A bare-metal host
    returns ``(False, "bare metal")``.
    """
    if runner.exists("/.dockerenv"):
        return True, "docker"
    if runner.exists("/run/.containerenv"):
        return True, "podman"
    result: CommandResult = runner.run(["systemd-detect-virt"], timeout=10)
    if result.returncode == 0:
        kind = result.stdout.strip()
        if kind and kind != "none":
            return True, kind
    try:
        cgroup = runner.read_text("/proc/1/cgroup")
    except OSError:
        cgroup = ""
    lowered = cgroup.lower()
    for marker in ("docker", "podman", "libpod", "kubepods", "containerd", "lxc"):
        if marker in lowered:
            return True, marker
    return False, "bare metal"


def check_disposable(env: dict, runner: Runner) -> tuple[bool, str]:
    """The safety gate: refuse unless this is a disposable container or VM.

    Both conditions must hold: ``SPOT_HARNESS_DISPOSABLE=1`` in the environment, and a
    root that is a container or a VM. This is what stops the harness from running on a
    developer's laptop or a production server.
    """
    if env.get("SPOT_HARNESS_DISPOSABLE") != "1":
        return False, "SPOT_HARNESS_DISPOSABLE is not set to 1"
    ok, kind = detect_disposable_root(runner)
    if not ok:
        return False, f"the host root is not a container or a VM ({kind})"
    return True, kind


def execute(
    run_plan: RunPlan,
    runner: Runner,
    *,
    runtime: str,
    binary: str,
    matrix_dir: str = ".",
    build_images: bool = True,
    ready_timeout: int = 60,
) -> list[TargetReport]:
    """Run every planned test and return one :class:`TargetReport` per target.

    For each target the image is pulled or built once, the container is started detached,
    and each runnable test is run with ``exec`` on its own (``--exact``), so a crash in one
    test cannot hide another's result. The container is always removed in a ``finally``.
    Skipped tests become JUnit ``<skipped>`` cases with their reason.
    """
    reports: list[TargetReport] = []
    for target_plan in run_plan.targets:
        report = TargetReport(target=target_plan.target.name)
        reports.append(report)
        setup_error = ""
        started = False

        if build_images:
            image = runner.run(image_argv(runtime, target_plan.target, matrix_dir))
            if not image.ok:
                setup_error = (
                    f"could not prepare the image: {' '.join(image.argv)} exited "
                    f"{image.returncode}: {image.stderr.strip() or image.stdout.strip()}"
                )

        name = container_name(target_plan.target.name)
        if not setup_error:
            created = runner.run(
                container_create_argv(runtime, target_plan.target, binary, name)
            )
            if created.ok:
                started = True
            else:
                setup_error = (
                    "could not start the container: "
                    f"{created.stderr.strip() or created.stdout.strip()}"
                )

        if setup_error:
            report.error = setup_error

        try:
            if started and target_plan.target.gives("systemd"):
                if not wait_for_systemd(runner, runtime, name, timeout=ready_timeout):
                    report.error = "systemd did not become ready in the container"
                    setup_error = report.error

            for decision in target_plan.decisions:
                if not decision.run:
                    report.results.append(
                        TestResult(decision.test.name, SKIPPED, decision.reason)
                    )
                    continue
                if setup_error:
                    report.results.append(TestResult(decision.test.name, ERROR, setup_error))
                    continue
                argv = container_exec_argv(runtime, name, decision.test.name)
                result = runner.run(argv)
                report.results.append(_result_from_run(decision.test.name, result))
        finally:
            if started:
                runner.run(container_remove_argv(runtime, name))
    return reports


def _result_from_run(name: str, result: CommandResult) -> TestResult:
    """Turn one container run into a test result."""
    combined = (result.stdout or "") + "\n" + (result.stderr or "")
    parsed = parse_libtest_output(combined, run_duration(combined))
    if name in parsed:
        parsed[name].duration = run_duration(combined)
        return parsed[name]
    if result.returncode == 0:
        # libtest prints "running 0 tests" and exits 0 when the name filter matches nothing
        # (a renamed or mistyped test): that must be a failure, never a silent pass.
        return TestResult(
            name,
            ERROR,
            "no libtest result line for this test (the name matched nothing?); exit 0",
            run_duration(combined),
        )
    detail = (result.stderr or result.stdout or "").strip()
    if len(detail) > 4000:
        detail = detail[:4000] + "\n… (truncated)"
    return TestResult(
        name,
        ERROR,
        f"{' '.join(result.argv[:2])} exited {result.returncode}: {detail}",
        run_duration(combined),
    )


def render_table(run_plan: RunPlan, reports: list[TargetReport]) -> str:
    """A human-readable matrix: one row per test, one column per target."""
    by_target = {report.target: report for report in reports}
    target_names = [target.target.name for target in run_plan.targets]
    test_names = (
        [decision.test.name for decision in run_plan.targets[0].decisions]
        if run_plan.targets
        else []
    )
    if not target_names:
        return "no targets selected"

    width = max([len("test")] + [len(name) for name in test_names]) + 2
    header = "test".ljust(width) + "".join(name.ljust(18) for name in target_names)
    lines = [header, "-" * len(header)]

    for test_name in test_names:
        row = test_name.ljust(width)
        for target_name in target_names:
            report = by_target.get(target_name)
            status = _cell(report, test_name)
            row += status.ljust(18)
        lines.append(row)

    lines.append("")
    lines.append("legend: PASS / FAIL / SKIP-needs-vm / SKIP-needs-cap")
    for target_name in target_names:
        report = by_target.get(target_name)
        if report is None:
            continue
        passed, failed, skipped = report.counts()
        suffix = f"  error: {report.error}" if report.error else ""
        lines.append(
            f"{target_name}: {passed} passed, {failed} failed, {skipped} skipped{suffix}"
        )
    return "\n".join(lines)


def _cell(report: TargetReport | None, test_name: str) -> str:
    if report is None:
        return "-"
    for result in report.results:
        if result.name != test_name:
            continue
        if result.status == PASSED:
            return "PASS"
        if result.status in (FAILED, ERROR):
            return "FAIL"
        if result.status == SKIPPED:
            return "SKIP-vm" if "needs vm" in result.detail else "SKIP-cap"
    return "-"


def has_failures(reports: list[TargetReport]) -> bool:
    """Whether any target has a failed or errored test, or a setup error."""
    for report in reports:
        if report.error:
            return True
        if any(result.status in (FAILED, ERROR) for result in report.results):
            return True
    return False


@dataclass
class ExecutionSummary:
    """The result of a whole run, for the CLI to report and write out."""

    plan: RunPlan
    reports: list[TargetReport] = field(default_factory=list)
