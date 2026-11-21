"""Scenario: removal, and the purge that follows it on Debian.

The packaging policy this asserts (``packaging/linux``): ``remove`` keeps the configuration
and the data; ``purge`` (DEB) or ``erase`` (RPM) takes the unit and the binary with it; the
``spot`` account is left behind because no maintainer script deletes an account that may own
data. A modified RPM config is saved as ``.rpmsave`` instead of being deleted outright, so
even an erase does not destroy the operator's edit.

Two canary files — one in ``/etc/spot``, one in ``/var/lib/spot`` — prove the last promise:
nothing the package did not install is ever deleted.
"""

from __future__ import annotations

import commands
from context import Context, Step
from distros import PACKAGING_DIR, Distro

NAME = "removal"
PROVES = (
    "remove keeps the config and the data, purge/erase removes the unit and the binary, "
    "the service account follows the stated policy, and no file the package did not "
    "install is deleted"
)
REQUIRES = ("clean_install",)

#: A file in the config directory that the package does not own.
ETC_CANARY = "/etc/spot/verify-canary"
#: A file in the state directory that the package does not own.
DATA_CANARY_NAME = "verify-canary"
DATA_MARKER_NAME = "verify-data"
DATA_CONTENT = "spot packaging verify: data written before the removal\n"
CANARY_CONTENT = "spot packaging verify: not installed by the package\n"
#: The edited config the RPM has to save as ``.rpmsave`` when it is erased.
EDITED_NODE_ID = 'node_id = "node-verify"'


def plan(distro: Distro, package: str = "", newer: str = "") -> tuple[Step, ...]:
    """The commands this scenario runs, for ``--dry-run``."""
    del package, newer
    steps = [
        Step(
            "canaries",
            ("install", "-m", "0644", "<work>/<canary>", ETC_CANARY),
            "exit 0; the canaries are not owned by the package",
        ),
        Step("remove", tuple(commands.remove(distro)), "exit 0"),
        Step("binary gone", ("test", "-e", "/usr/bin/spot"), "exit 1 (the file is gone)"),
        Step("unit gone", ("test", "-e", distro.unit_path), "exit 1 (the file is gone)"),
        Step(
            "config",
            ("test", "-e", distro.config_path),
            (
                "exit 0 (kept)"
                if distro.policy.remove_keeps_config
                else f"exit 1, saved as {distro.config_path}{distro.policy.config_backup_suffix}"
            ),
        ),
        Step(
            "data survives",
            tuple(commands.cat(f"{distro.data_dir}/{DATA_MARKER_NAME}")),
            "the marker content is unchanged",
        ),
        Step(
            "canaries survive",
            ("test", "-e", ETC_CANARY),
            "exit 0 (a file the package did not install)",
        ),
        Step(
            "service account",
            tuple(commands.getent("passwd", distro.user)),
            (
                "deleted"
                if distro.policy.removes_service_user
                else "still present (no scriptlet deletes an account that may own data)"
            ),
        ),
    ]
    if distro.policy.separate_purge:
        steps.append(Step("purge", tuple(commands.purge(distro)), "exit 0"))
        steps.append(
            Step("config gone", ("test", "-e", distro.config_path), "exit 1 (purged)")
        )
        steps.append(
            Step("canary survives purge", ("test", "-e", ETC_CANARY), "exit 0")
        )
    return tuple(steps)


def _edited_config_text() -> str:
    """The shipped template with the operator edit, which an erase has to save aside."""
    template = (PACKAGING_DIR / "etc" / "spot.toml").read_text(encoding="utf-8")
    return template.replace('node_id = "node-0"', EDITED_NODE_ID)


def run(context: Context) -> str:
    """Remove, then purge on Debian; returns a one-line detail."""
    distro = context.distro
    installed = context.installed_version()
    context.check(
        bool(installed),
        "the package is not installed, so there is nothing to remove "
        "(clean_install must pass first)",
    )

    data_marker = f"{distro.data_dir}/{DATA_MARKER_NAME}"
    data_canary = f"{distro.data_dir}/{DATA_CANARY_NAME}"

    # A modified config, so an RPM erase has to save it as .rpmsave rather than drop a
    # file the operator changed.
    edited = context.exec(
        commands.install_file(
            context.work_file("spot.toml.edited", _edited_config_text()),
            distro.config_path,
            mode="0640",
            owner="root",
            group=distro.group,
        )
    )
    context.check_ok(edited, "could not edit the config before the removal")

    for destination, mode, owner, content in (
        (ETC_CANARY, "0644", "root", CANARY_CONTENT),
        (data_canary, "0640", distro.user, CANARY_CONTENT),
        (data_marker, "0640", distro.user, DATA_CONTENT),
    ):
        name = f"canary-{destination.rsplit('/', 1)[-1]}"
        written = context.exec(
            commands.install_file(
                context.work_file(name, content),
                destination,
                mode=mode,
                owner=owner,
                group="root" if owner == "root" else distro.group,
            )
        )
        context.check_ok(written, f"could not write {destination}")

    remove = context.exec(commands.remove(distro))
    context.check_ok(remove, "removing the package failed")
    _assert_removed(context, distro)

    kept_config = context.exec(commands.exists(distro.config_path))
    if distro.policy.remove_keeps_config:
        context.check(kept_config.ok, f"remove deleted {distro.config_path}")
    else:
        context.check(
            not kept_config.ok,
            f"remove left {distro.config_path} in place, unexpected on {distro.name}",
        )
        backup = distro.config_path + distro.policy.config_backup_suffix
        saved = context.exec(commands.exists(backup))
        context.check(
            saved.ok,
            f"the modified config was not saved as {backup}: an erase lost the operator edit",
        )

    _assert_data_intact(context, data_marker, DATA_CONTENT, data_canary)

    account = context.exec(commands.getent("passwd", distro.user))
    if distro.policy.removes_service_user:
        context.check(not account.ok, f"the {distro.user} account was not deleted")
    else:
        context.check(account.ok, f"the {distro.user} account was deleted on removal")

    purged = ""
    if distro.policy.separate_purge:
        purge = context.exec(commands.purge(distro))
        context.check_ok(purge, "purging the package failed")
        config_after = context.exec(commands.exists(distro.config_path))
        context.check(not config_after.ok, f"purge left {distro.config_path} in place")
        canary_after = context.exec(commands.exists(ETC_CANARY))
        context.check(
            canary_after.ok, "purge deleted a file in /etc/spot that it did not install"
        )
        _assert_data_intact(context, data_marker, DATA_CONTENT, data_canary)
        purged = "; purged, config deleted and canaries kept"

    return (
        f"remove kept the config and the data on {distro.name}"
        if distro.policy.remove_keeps_config
        else f"remove dropped the binary and the unit, config saved as "
        f"{distro.config_path}{distro.policy.config_backup_suffix}"
    ) + purged


def _assert_removed(context: Context, distro: Distro) -> None:
    """The binary, the unit and the package record are gone."""
    for path in ("/usr/bin/spot", distro.unit_path):
        present = context.exec(commands.exists(path))
        context.check(not present.ok, f"the removal left {path} in place")
    record = context.exec(commands.is_installed(distro))
    context.check(
        not commands.package_is_installed(distro, record.stdout),
        f"the package is still recorded as installed ({record.stdout.strip()!r})",
    )


def _assert_data_intact(
    context: Context, marker: str, expected: str, canary: str
) -> None:
    """The state marker is unchanged and the foreign canary survived."""
    content = context.exec(commands.cat(marker))
    context.check_ok(content, f"the state marker {marker} is gone")
    context.check(content.stdout == expected, f"the content of {marker} changed")
    survived = context.exec(commands.exists(canary))
    context.check(survived.ok, f"the removal deleted {canary}, a file it did not install")
