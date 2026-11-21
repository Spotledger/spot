"""Scenario: upgrade version N to version N+1 under a running service.

What has to survive an upgrade: the operator's edit to ``/etc/spot/spot.toml`` (``noreplace``
on RPM, a conffile on DEB, and a ``.rpmnew``/``.dpkg-dist`` only when the shipped template
itself changed), the data under ``/var/lib/spot``, and — for the distributions whose
scriptlets do it — a single restart of the service that was running.

The restart expectation is per distro (:class:`distros.Policy`) because the packaging
differs: ``%postun`` runs ``%systemd_postun_with_restart`` for the RPM, while the DEB that
``tools/packaging/build.py`` builds carries no debhelper snippet and therefore restarts
nothing. The scenario asserts the stated behaviour, so a change in either direction is a
failure rather than a surprise.
"""

from __future__ import annotations

import commands
from context import Context, Step
from distros import HEALTH_POLL_S, HEALTH_TIMEOUT_S, PACKAGING_DIR, RESTART_BOUND_S, Distro

NAME = "upgrade"
PROVES = (
    "installing N+1 over a running N keeps the edited config and the state marker, "
    "installs the new version and restarts the service exactly as the package scriptlets "
    "promise"
)
REQUIRES = ("clean_install",)

#: The value the scenario writes into the config, to prove the edit survived.
EDITED_NODE_ID = 'node_id = "node-verify"'
#: The line it replaces in the shipped template.
TEMPLATE_NODE_ID = 'node_id = "node-0"'
#: The marker file the scenario puts in the state directory.
MARKER_NAME = "verify-marker"
MARKER_CONTENT = "spot packaging verify: state written before the upgrade\n"
#: How long the restart is watched for a second, unexpected restart (seconds).
SETTLE_S = 2
#: Poll interval while watching the restarted process (seconds).
POLL_S = 1
#: The backup a changed config template produces on each distribution.
BACKUP_SUFFIX = {True: ".rpmnew", False: ".dpkg-dist"}


def plan(distro: Distro, package: str = "", newer: str = "") -> tuple[Step, ...]:
    """The commands this scenario runs, for ``--dry-run``."""
    del package
    return (
        Step(
            "installed version",
            tuple(commands.installed_version(distro)),
            "version N, the older artefact",
        ),
        Step(
            "state marker",
            tuple(
                commands.install_file(
                    "<work>/" + MARKER_NAME,
                    f"{distro.data_dir}/{MARKER_NAME}",
                    mode="0640",
                    owner=distro.user,
                    group=distro.group,
                )
            ),
            "exit 0",
        ),
        Step(
            "edit config",
            tuple(
                commands.install_file(
                    "<work>/spot.toml.edited",
                    distro.config_path,
                    mode="0640",
                    owner="root",
                    group=distro.group,
                )
            ),
            "exit 0; " + EDITED_NODE_ID,
        ),
        Step("start", tuple(commands.start(distro.unit_name)), "exit 0"),
        Step(
            "health",
            tuple(commands.health(distro.health_url)),
            f"exit 0 within {HEALTH_TIMEOUT_S} s",
        ),
        Step("main pid before", tuple(commands.unit_show("MainPID")), "a non-zero MainPID"),
        Step(
            "install N+1",
            tuple(commands.install(newer or "<package N+1>", distro)),
            "exit 0",
        ),
        Step(
            "installed version",
            tuple(commands.installed_version(distro)),
            "version N+1",
        ),
        Step(
            "config kept",
            tuple(commands.cat(distro.config_path)),
            EDITED_NODE_ID + " is still there",
        ),
        Step(
            "state kept",
            tuple(commands.cat(f"{distro.data_dir}/{MARKER_NAME}")),
            "the marker content is unchanged",
        ),
        Step(
            "service",
            tuple(commands.unit_show("ActiveState", "MainPID")),
            (
                "a different MainPID, active (RPM)"
                if distro.policy.restart_on_upgrade
                else "the same MainPID, active (no restart snippet in this .deb)"
            ),
        ),
        Step(
            "config backup",
            tuple(commands.exists(distro.config_path + BACKUP_SUFFIX[distro.is_rpm])),
            "absent while the shipped template is unchanged (recorded, not required)",
        ),
    )


def _edited_config_text() -> str:
    """The shipped template with the operator edit applied, as the scenario writes it."""
    template = (PACKAGING_DIR / "etc" / "spot.toml").read_text(encoding="utf-8")
    if TEMPLATE_NODE_ID not in template:
        raise AssertionError(
            f"the config template no longer contains {TEMPLATE_NODE_ID}; update this scenario"
        )
    return template.replace(TEMPLATE_NODE_ID, EDITED_NODE_ID)


def run(context: Context) -> str:
    """Upgrade from the older artefact to the newer one; returns a one-line detail."""
    distro = context.distro
    old, new = context.pair()

    installed = context.installed_version()
    context.check(
        installed == old.display_version,
        f"the installed version is {installed!r}, expected {old.display_version!r}",
    )

    marker_source = context.work_file(MARKER_NAME, MARKER_CONTENT)
    marker = context.exec(
        commands.install_file(
            marker_source,
            f"{distro.data_dir}/{MARKER_NAME}",
            mode="0640",
            owner=distro.user,
            group=distro.group,
        )
    )
    context.check_ok(marker, "could not write the state marker")

    config_source = context.work_file("spot.toml.edited", _edited_config_text())
    edited = context.exec(
        commands.install_file(
            config_source,
            distro.config_path,
            mode="0640",
            owner="root",
            group=distro.group,
        )
    )
    context.check_ok(edited, "could not edit the config before the upgrade")

    start = context.exec(commands.start(distro.unit_name))
    context.check_ok(start, "could not start the service before the upgrade")
    context.poll_until(
        lambda: context.exec(commands.health(distro.health_url)).ok,
        deadline_s=HEALTH_TIMEOUT_S,
        interval_s=HEALTH_POLL_S,
        what=f"the health endpoint {distro.health_url} before the upgrade",
    )
    pid_before = context.unit_properties("MainPID").get("MainPID", "0")
    context.check(
        pid_before.isdigit() and pid_before != "0",
        f"systemd reports MainPID={pid_before!r} before the upgrade",
    )

    upgrade = context.exec(commands.install(context.container_path(new), distro))
    context.check_ok(upgrade, f"installing {new.filename} over {old.filename} failed")

    after = context.installed_version()
    context.check(
        after == new.display_version,
        f"the installed version is {after!r} after the upgrade, expected {new.display_version!r}",
    )

    config = context.exec(commands.cat(distro.config_path))
    context.check_ok(config, "the config is gone after the upgrade")
    context.check(
        EDITED_NODE_ID in config.stdout,
        "the upgrade overwrote the edited config",
    )

    marker_after = context.exec(commands.cat(f"{distro.data_dir}/{MARKER_NAME}"))
    context.check_ok(marker_after, "the state marker is gone after the upgrade")
    context.check(
        marker_after.stdout == MARKER_CONTENT,
        "the state marker changed during the upgrade",
    )

    backup = distro.config_path + BACKUP_SUFFIX[distro.is_rpm]
    backup_result = context.exec(commands.exists(backup))
    context.note(f"{backup}: {'present' if backup_result.ok else 'absent'}")

    restart_after = 0
    if distro.policy.restart_on_upgrade:
        restart_after = context.poll_until(
            lambda: _restarted(context, pid_before),
            deadline_s=RESTART_BOUND_S,
            interval_s=POLL_S,
            what=f"the restarted service after upgrading (pid {pid_before})",
        )
        pid_after = context.unit_properties("MainPID").get("MainPID", "0")
        for _ in range(SETTLE_S):
            context.sleep(POLL_S)
        settled = context.unit_properties("ActiveState", "MainPID")
        context.check(
            settled.get("MainPID") == pid_after and settled.get("ActiveState") == "active",
            "the service restarted more than once or is not active after the upgrade",
        )
        restarted = f"restarted once ({pid_before} -> {pid_after}) after {restart_after} s"
    else:
        state = context.unit_properties("ActiveState", "MainPID")
        pid_after = state.get("MainPID", "0")
        context.check(
            state.get("ActiveState") == "active",
            f"the service is {state.get('ActiveState')!r} after the upgrade",
        )
        context.check(
            pid_after == pid_before,
            f"the service restarted on this distro ({pid_before} -> {pid_after}) but its "
            "packaging has no restart snippet; the policy in distros.py is wrong",
        )
        restarted = f"not restarted (pid {pid_before}), as this .deb has no restart snippet"

    backup_note = "present" if backup_result.ok else "absent"
    return (
        f"{old.version} -> {new.version}; edited config kept; state marker intact; "
        f"service {restarted}; {backup.rsplit('/', 1)[-1]} {backup_note}"
    )


def _restarted(context: Context, pid_before: str) -> bool:
    """Whether a different, live main process is serving (the restart probe)."""
    state = context.unit_properties("ActiveState", "MainPID")
    candidate = state.get("MainPID", "0")
    return (
        candidate.isdigit()
        and candidate != "0"
        and candidate != pid_before
        and state.get("ActiveState") == "active"
    )
