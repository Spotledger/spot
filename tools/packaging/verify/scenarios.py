"""The scenario table: what runs, in which order, and what it proves.

The table is data. Every entry is one function in its own module with its own test, and the
order is the order on this fresh container: install first, then the service, then the
upgrade, the downgrade attempt and finally the removal. A scenario whose prerequisite did not
pass is skipped with that reason, never run against a half-installed system.

``--list`` prints the table, ``--dry-run`` prints the exact argument lists that would be run
(including the real artefact paths when the package directory is there), and neither starts
a container.
"""

from __future__ import annotations

import json
from pathlib import Path

import packages
import scenario_container
import scenario_downgrade
import scenario_install
import scenario_removal
import scenario_service
import scenario_upgrade
from context import PKG_MOUNT, Scenario, Step
from distros import Distro

#: The table, in execution order. A shared scenario runs once per invocation.
SCENARIOS: tuple[Scenario, ...] = (
    Scenario(
        name=scenario_install.NAME,
        proves=scenario_install.PROVES,
        run=scenario_install.run,
        plan=scenario_install.plan,
        requires=(),
    ),
    Scenario(
        name=scenario_service.NAME,
        proves=scenario_service.PROVES,
        run=scenario_service.run,
        plan=scenario_service.plan,
        requires=scenario_service.REQUIRES,
    ),
    Scenario(
        name=scenario_upgrade.NAME,
        proves=scenario_upgrade.PROVES,
        run=scenario_upgrade.run,
        plan=scenario_upgrade.plan,
        requires=scenario_upgrade.REQUIRES,
    ),
    Scenario(
        name=scenario_downgrade.NAME,
        proves=scenario_downgrade.PROVES,
        run=scenario_downgrade.run,
        plan=scenario_downgrade.plan,
        requires=scenario_downgrade.REQUIRES,
    ),
    Scenario(
        name=scenario_removal.NAME,
        proves=scenario_removal.PROVES,
        run=scenario_removal.run,
        plan=scenario_removal.plan,
        requires=scenario_removal.REQUIRES,
    ),
    Scenario(
        name=scenario_container.NAME,
        proves=scenario_container.PROVES,
        run=scenario_container.run,
        plan=scenario_container.plan,
        requires=scenario_container.REQUIRES,
        shared=scenario_container.SHARED,
    ),
)

#: The scenario names in execution order, for the tests and for ``--list``.
NAMES: tuple[str, ...] = tuple(scenario.name for scenario in SCENARIOS)


def get(name: str) -> Scenario:
    """The scenario called ``name``.

    Raises:
        KeyError: when no scenario has that name.
    """
    for scenario in SCENARIOS:
        if scenario.name == name:
            return scenario
    raise KeyError(f"unknown scenario: {name}")


def for_distro(distro: Distro) -> tuple[Scenario, ...]:
    """Every scenario that applies to ``distro``, shared ones included."""
    del distro
    return SCENARIOS


def distro_scenarios(distro: Distro) -> tuple[Scenario, ...]:
    """The scenarios that run inside ``distro``'s container (not the shared ones)."""
    return tuple(scenario for scenario in for_distro(distro) if not scenario.shared)


def shared_scenarios() -> tuple[Scenario, ...]:
    """The scenarios that run once for the whole invocation, on the host."""
    return tuple(scenario for scenario in SCENARIOS if scenario.shared)


def select(names: list[str] | None) -> tuple[Scenario, ...]:
    """The selected scenarios in table order; every scenario when ``names`` is empty."""
    if not names:
        return SCENARIOS
    wanted = {get(name).name for name in names}
    return tuple(scenario for scenario in SCENARIOS if scenario.name in wanted)


def container_paths(
    distro: Distro, artefacts: tuple[packages.Artefact, ...]
) -> tuple[str, str]:
    """The container paths of the older and newer artefact, with placeholders when absent."""
    pair = packages.upgrade_pair(artefacts, distro.package_format)
    if pair is not None:
        return f"{PKG_MOUNT}/{pair[0].filename}", f"{PKG_MOUNT}/{pair[1].filename}"
    newest = packages.latest(artefacts, distro.package_format)
    if newest is not None:
        path = f"{PKG_MOUNT}/{newest.filename}"
        return path, path
    return "<package>", "<package N+1>"


def steps_for(
    scenario: Scenario,
    distro: Distro,
    artefacts: tuple[packages.Artefact, ...],
) -> tuple[Step, ...]:
    """The steps ``scenario`` would run on ``distro`` with these artefacts."""
    package, newer = container_paths(distro, artefacts)
    return scenario.plan(distro, package, newer)


def describe(distro_order: tuple[Distro, ...]) -> str:
    """The ``--list`` output: the distros, the scenarios and what each one proves."""
    lines = ["distributions:"]
    for distro in distro_order:
        lines.append(
            f"  {distro.name:14} family={distro.family:6} format={distro.package_format:3} "
            f"manager={distro.package_manager:7} image={distro.image_tag}"
        )
        lines.append(
            f"  {'':14} Containerfile: {_relative(distro.containerfile)}"
        )
    lines.append("")
    lines.append("scenarios (in execution order):")
    for scenario in SCENARIOS:
        needs = ", ".join(scenario.requires) or "nothing"
        shared = " (shared: runs once, not per distro)" if scenario.shared else ""
        lines.append(f"  {scenario.name:16} requires: {needs}{shared}")
        for wrapped in _wrap(scenario.proves, 76):
            lines.append(f"  {'':16} {wrapped}")
    lines.append("")
    lines.append("per-distro expectations:")
    for distro in distro_order:
        policy = distro.policy
        lines.append(f"  {distro.name}:")
        lines.append(f"    upgrade restart: {'yes' if policy.restart_on_upgrade else 'no'} "
                     f"({policy.restart_reason})")
        lines.append(
            f"    remove keeps the config: {'yes' if policy.remove_keeps_config else 'no'}; "
            f"separate purge step: {'yes' if policy.separate_purge else 'no (erase)'}; "
            f"removal deletes the account: {'yes' if policy.removes_service_user else 'no'}"
        )
        lines.append(f"    downgrade: {policy.downgrade_expectation}")
    return "\n".join(lines)


def dry_run(
    distro_order: tuple[Distro, ...],
    artefacts: tuple[packages.Artefact, ...],
    selected: tuple[Scenario, ...] | None = None,
) -> str:
    """The ``--dry-run`` plan: the exact argument lists, with nothing executed."""
    chosen = selected if selected is not None else SCENARIOS
    lines = [
        "packaging verify plan (dry run: no image is built, no container is started)",
        "",
        f"package directory: {_package_summary(artefacts)}",
        "",
    ]
    for distro in distro_order:
        container = distro.container_name
        lines.append(f"{distro.name}  ({distro.package_manager}, {distro.package_format})")
        lines.append(
            "  container: podman run -d --name " + container + " --systemd=always "
            + distro.image_tag
        )
        for scenario in chosen:
            if scenario.shared:
                continue
            lines.append(f"  scenario {scenario.name}")
            for index, step in enumerate(
                steps_for(scenario, distro, artefacts), start=1
            ):
                lines.append(f"    {index}. {step.label}: {_argv(step.argv)}")
                lines.append(f"       expect: {step.expect}")
        lines.append("")

    shared = tuple(scenario for scenario in chosen if scenario.shared)
    if shared:
        lines.append("shared scenarios (once per invocation, on the host):")
        for scenario in shared:
            lines.append(f"  scenario {scenario.name}")
            for index, step in enumerate(
                steps_for(scenario, distro_order[0], artefacts), start=1
            ):
                lines.append(f"    {index}. {step.label}: {_argv(step.argv)}")
                lines.append(f"       expect: {step.expect}")
        lines.append("")
    return "\n".join(lines)


def _argv(argv: tuple[str, ...]) -> str:
    """Render an argument list as a JSON array, so it is visibly not a shell string."""
    return json.dumps(list(argv), ensure_ascii=False)


def _relative(path: Path) -> str:
    """A repository-relative path when possible, so the output does not depend on cwd."""
    try:
        return str(path.relative_to(Path(__file__).resolve().parents[3]))
    except ValueError:
        return str(path)


def _package_summary(artefacts: tuple[packages.Artefact, ...]) -> str:
    """How many artefacts were found, by file name, for the plan header."""
    if not artefacts:
        return "no artefact found (the scenarios that need one will skip)"
    return ", ".join(artefact.filename for artefact in artefacts)


def _wrap(text: str, width: int) -> list[str]:
    """Wrap ``text`` on spaces, so a long proof line stays readable in a terminal."""
    words = text.split()
    lines: list[str] = []
    current = ""
    for word in words:
        candidate = f"{current} {word}".strip()
        if len(candidate) > width and current:
            lines.append(current)
            current = word
        else:
            current = candidate
    if current:
        lines.append(current)
    return lines
