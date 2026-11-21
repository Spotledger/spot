"""Scenario: a clean install on a booted distribution.

What the package promises on a fresh host: the daemon, a service account that cannot log
in, a configuration file that is ``noreplace`` and readable only by ``root:spot``, a
systemd unit that ``systemd-analyze verify`` accepts — and a service that is installed but
deliberately **not** enabled and **not** started, because an administrator provisions a site
first.
"""

from __future__ import annotations

import commands
from context import Context, Step
from distros import Distro

NAME = "clean_install"
PROVES = (
    "the package installs the daemon, a service account without a login shell, a 0640 "
    "root:spot noreplace config and a unit that systemd-analyze accepts, without enabling "
    "or starting the service"
)
PLACEHOLDER = "<package>"


def plan(
    distro: Distro, package: str = PLACEHOLDER, newer: str = ""
) -> tuple[Step, ...]:
    """The commands this scenario runs, for ``--dry-run``.

    ``package``/``newer`` are the older and newer artefact paths the driver found; a clean
    install takes the older one, so the upgrade scenario after it has N -> N+1 to prove.
    """
    del newer
    return (
        Step("install", tuple(commands.install(package, distro)), "exit 0"),
        Step(
            "service account",
            tuple(commands.getent("passwd", distro.user)),
            f"exit 0, home {distro.data_dir}, shell disabled",
        ),
        Step(
            "service group",
            tuple(commands.getent("group", distro.group)),
            "exit 0",
        ),
        Step(
            "config mode",
            tuple(commands.stat("%a %U %G %n", distro.config_path)),
            f"640 root {distro.group} {distro.config_path}",
        ),
        Step(
            "config noreplace",
            tuple(commands.config_files(distro)),
            f"{distro.config_path} is marked noreplace",
        ),
        Step("unit file mode", tuple(commands.stat("%a %n", distro.unit_path)), "644"),
        Step("unit verify", tuple(commands.verify_unit(distro.unit_path)), "exit 0, no error"),
        Step(
            "not enabled",
            tuple(commands.is_enabled(distro.unit_name)),
            "not enabled (is-enabled prints disabled and exits 1)",
        ),
        Step(
            "not started",
            tuple(commands.is_active(distro.unit_name)),
            "not active (the package never starts the service)",
        ),
        Step(
            "state directory",
            tuple(commands.stat("%a %U %G %n", distro.data_dir)),
            f"750 {distro.user} {distro.group} {distro.data_dir}",
        ),
    )


def run(context: Context) -> str:
    """Install the package and assert every promise above; returns a one-line detail."""
    distro = context.distro
    artefact = context.first_package()
    package = context.container_path(artefact)

    install = context.exec(commands.install(package, distro))
    context.check_ok(install, f"installing {artefact.filename} failed")

    account = context.exec(commands.getent("passwd", distro.user))
    context.check_ok(account, f"the package did not create the {distro.user} account")
    fields = commands.passwd_fields(account.stdout)
    context.check(len(fields) >= 7, f"unreadable passwd entry for {distro.user}: {account.stdout!r}")
    shell = fields[6]
    context.check(
        commands.login_shell_is_disabled(shell),
        f"the {distro.user} account has a login shell: {shell}",
    )
    context.check(
        fields[5] == distro.data_dir,
        f"the {distro.user} home is {fields[5]}, expected {distro.data_dir}",
    )

    group = context.exec(commands.getent("group", distro.group))
    context.check_ok(group, f"the package did not create the {distro.group} group")

    config = context.exec(commands.stat("%a %U %G %n", distro.config_path))
    expected_config = f"640 root {distro.group} {distro.config_path}"
    context.check(
        config.ok and config.stdout.strip() == expected_config,
        f"the config is {config.stdout.strip()!r}, expected {expected_config!r}",
    )

    flags = context.exec(commands.config_files(distro))
    context.check_ok(flags, "could not read the package's config file list")
    context.check(
        commands.config_is_noreplace(distro, flags.stdout, distro.config_path),
        f"the package does not mark {distro.config_path} as noreplace",
    )

    unit = context.exec(commands.stat("%a %n", distro.unit_path))
    context.check(unit.ok, f"the unit is not installed at {distro.unit_path}")
    context.check(
        unit.stdout.split()[0] == "644",
        f"the unit is not installed 0644: {unit.stdout.strip()!r}",
    )

    verify = context.exec(commands.verify_unit(distro.unit_path))
    context.check_ok(verify, "systemd-analyze verify rejected the installed unit")
    context.check(
        not verify.stderr.strip(),
        f"systemd-analyze verify warned about the unit: {verify.stderr.strip()[:200]}",
    )

    enabled = context.exec(commands.is_enabled(distro.unit_name))
    context.check(
        enabled.stdout.strip() != "enabled",
        "the package enabled the service; enabling is the administrator's step",
    )

    active = context.exec(commands.is_active(distro.unit_name))
    context.check(
        active.stdout.strip() != "active",
        "the package started the service; starting is the administrator's step",
    )

    state = context.exec(commands.stat("%a %U %G %n", distro.data_dir))
    expected_state = f"750 {distro.user} {distro.group} {distro.data_dir}"
    context.check(
        state.ok and state.stdout.strip() == expected_state,
        f"the state directory is {state.stdout.strip()!r}, expected {expected_state!r}",
    )

    return (
        f"{artefact.filename} installed; {distro.user} without a login shell; "
        f"{distro.config_path} 0640 root:{distro.group} noreplace; unit loaded, "
        "disabled and inactive"
    )
