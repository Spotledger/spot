"""Scenario: an explicit start, the health endpoint, a bounded stop and a restart.

The unit is ``Type=simple`` and the binary has no ``sd_notify`` (see
``packaging/linux/README.md``), so readiness is the public health endpoint, not the service
manager. This scenario proves the three things an operator depends on: a start that becomes
healthy within a stated timeout, a crash that systemd restarts because ``Restart=`` says so,
and a stop that finishes inside the unit's own ``TimeoutStopSec``.
"""

from __future__ import annotations

import commands
from context import Context, Step
from distros import HEALTH_POLL_S, HEALTH_TIMEOUT_S, RESTART_BOUND_S, STOP_MARGIN_S, Distro

NAME = "start_and_health"
PROVES = (
    "an explicit systemctl start serves the health endpoint on loopback inside the stated "
    "timeout, a SIGKILL of the main process is restarted per Restart=, and a stop fits the "
    "unit's TimeoutStopSec"
)
REQUIRES = ("clean_install",)

#: Poll interval while waiting for the restarted main process (seconds).
RESTART_POLL_S = 1
#: The stop budget assumed when the unit does not report TimeoutStopSec (seconds).
FALLBACK_STOP_BUDGET_S = 40


def plan(distro: Distro, package: str = "", newer: str = "") -> tuple[Step, ...]:
    """The commands this scenario runs, for ``--dry-run``."""
    del package, newer
    return (
        Step("start", tuple(commands.start(distro.unit_name)), "exit 0"),
        Step(
            "health",
            tuple(commands.health(distro.health_url)),
            f"exit 0 within {HEALTH_TIMEOUT_S} s, polled every {HEALTH_POLL_S} s",
        ),
        Step(
            "unit state",
            tuple(commands.unit_show("ActiveState", "Restart", "MainPID")),
            "ActiveState=active, Restart=on-failure, a non-zero MainPID",
        ),
        Step("kill the main process", ("kill", "-9", "<MainPID>"), "the process dies"),
        Step(
            "restart",
            tuple(commands.unit_show("ActiveState", "MainPID")),
            f"a different, non-zero MainPID within {RESTART_BOUND_S} s",
        ),
        Step(
            "stop budget",
            tuple(commands.unit_show("TimeoutStopUSec")),
            "the stop budget the unit declares",
        ),
        Step(
            "stop",
            tuple(commands.stop(distro.unit_name)),
            "exit 0 and finishes inside TimeoutStopSec",
        ),
    )


def _main_pid(context: Context, pid: str) -> bool:
    """Whether a different, live main process is serving (the restart probe)."""
    state = context.unit_properties("ActiveState", "MainPID")
    candidate = state.get("MainPID", "0")
    return (
        candidate.isdigit()
        and candidate != "0"
        and candidate != pid
        and state.get("ActiveState") == "active"
    )


def run(context: Context) -> str:
    """Start, probe health, kill and stop; returns a one-line detail."""
    distro = context.distro

    start = context.exec(commands.start(distro.unit_name))
    context.check_ok(start, "systemctl start failed")

    healthy_after = context.poll_until(
        lambda: context.exec(commands.health(distro.health_url)).ok,
        deadline_s=HEALTH_TIMEOUT_S,
        interval_s=HEALTH_POLL_S,
        what=f"the health endpoint {distro.health_url}",
    )

    state = context.unit_properties("ActiveState", "Restart", "MainPID")
    context.check(
        state.get("ActiveState") == "active",
        f"the unit is {state.get('ActiveState')!r} after a start",
    )
    context.check(
        state.get("Restart") == "on-failure",
        f"Restart= is {state.get('Restart')!r}, expected 'on-failure'",
    )
    pid = state.get("MainPID", "0")
    context.check(pid.isdigit() and pid != "0", f"systemd reports MainPID={pid!r}")

    killed = context.exec(commands.kill(pid))
    context.check_ok(killed, f"could not kill the main process {pid}")

    restarted_after = context.poll_until(
        lambda: _main_pid(context, pid),
        deadline_s=RESTART_BOUND_S,
        interval_s=RESTART_POLL_S,
        what=f"a restarted main process after killing {pid}",
    )
    new_pid = context.unit_properties("MainPID").get("MainPID", "0")

    budget = commands.parse_microseconds(
        context.unit_properties("TimeoutStopUSec").get("TimeoutStopUSec", "")
    )
    if budget <= 0:
        budget = FALLBACK_STOP_BUDGET_S

    started_ms = context.clock.now_ms()
    stop = context.exec(commands.stop(distro.unit_name))
    stop_ms = context.clock.now_ms() - started_ms
    context.check_ok(stop, "systemctl stop failed")
    context.check(
        stop_ms <= (budget + STOP_MARGIN_S) * 1000,
        f"the stop took {stop_ms // 1000} s, over the {budget} s budget plus {STOP_MARGIN_S} s",
    )

    return (
        f"healthy after {healthy_after} s; systemd replaced pid {pid} with {new_pid} after "
        f"{restarted_after} s; stop took {stop_ms // 1000} s of {budget} s"
    )
