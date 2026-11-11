#!/usr/bin/env python3
"""Prove the Linux packages on real distributions, in throwaway systemd containers.

    run.py [--distro fedora-44|almalinux-10|ubuntu-2404|all] [--package-dir DIR]
           [--binary PATH] [--scenario NAME] [--keep] [--dry-run] [--list] [--report FILE]

This driver builds no package: it takes the artefacts ``tools/packaging/build.py`` wrote,
mounts them into one ``--systemd=always`` container per distribution and runs the scenario
table inside it. ``--list`` prints what is proved and where; ``--dry-run`` prints the exact
argument lists without starting anything. When podman is absent the driver exits **77**
("skipped") with a clear message, so CI without podman is not red.

Exit codes: 0 everything passed or skipped, 1 a scenario failed or errored, 2 bad usage,
77 podman is not installed. Standard library only.
"""

from __future__ import annotations

import argparse
import sys
import time
from pathlib import Path

import distros
import packages
import report
import scenarios
from driver import Driver, cgroup_v2_present, default_clock
from runtime import SubprocessRunner

#: The container runtime this driver uses; the runner user is not root and not in `docker`.
RUNTIME = "podman"

EXIT_OK = 0
EXIT_FAIL = 1
EXIT_USAGE = 2
#: What a test harness means by "skipped": the tool this needs is not installed.
EXIT_SKIP = 77

#: The default package directory, the same default ``build.py`` writes to.
DEFAULT_PACKAGE_DIR = Path("dist")


def build_parser() -> argparse.ArgumentParser:
    """The argument parser."""
    parser = argparse.ArgumentParser(
        prog="packaging-verify",
        description=(
            "Run the packaging scenarios on real distributions inside systemd containers. "
            "It builds nothing: pass the packages tools/packaging/build.py produced."
        ),
    )
    parser.add_argument(
        "--distro",
        choices=distros.CHOICES,
        default=distros.ALL,
        help="which distribution to prove (default: %(default)s)",
    )
    parser.add_argument(
        "--package-dir",
        type=Path,
        default=DEFAULT_PACKAGE_DIR,
        help="the directory build.py wrote the .rpm/.deb files to (default: %(default)s)",
    )
    parser.add_argument(
        "--binary",
        type=Path,
        default=None,
        help="the release binary the container-image scenario builds with",
    )
    parser.add_argument(
        "--scenario",
        action="append",
        choices=scenarios.NAMES,
        metavar="NAME",
        help="only this scenario (repeatable); default is the whole table",
    )
    parser.add_argument(
        "--keep",
        action="store_true",
        help="keep the containers and the fixture directory for inspection",
    )
    parser.add_argument(
        "--dry-run",
        action="store_true",
        help="print the plan and the argument lists, start nothing",
    )
    parser.add_argument(
        "--list",
        action="store_true",
        help="list the distributions, the scenarios and the per-distro expectations",
    )
    parser.add_argument(
        "--report",
        type=Path,
        default=None,
        metavar="FILE",
        help="write the JSON result report to FILE",
    )
    return parser


def main(argv: list[str] | None = None) -> int:
    """The entry point; returns the process exit code."""
    args = build_parser().parse_args(argv)
    distro_order = distros.select(args.distro)
    selected = scenarios.select(args.scenario)

    if args.list:
        print(scenarios.describe(distro_order))
        return EXIT_OK

    package_dir = args.package_dir.expanduser()
    artefacts = packages.discover(package_dir)

    if args.dry_run:
        print(scenarios.dry_run(distro_order, artefacts, selected))
        return EXIT_OK

    runner = SubprocessRunner()
    binary = args.binary.expanduser() if args.binary is not None else None
    if binary is not None and not binary.is_file():
        print(f"error: the binary {binary} does not exist", file=sys.stderr)
        return EXIT_USAGE
    if runner.which(RUNTIME) is None:
        print(
            "SKIP packaging verify: podman is not installed (or not on PATH), so no "
            "distribution can be started.\n"
            "    Install podman 4.4 or newer and run it rootless; see "
            "packaging/linux/verify/README.md.",
            file=sys.stderr,
        )
        return EXIT_SKIP

    if binary is None and any(scenario.name == "container_image" for scenario in selected):
        print(
            "note: no --binary given, so the container-image scenario will be skipped.",
            file=sys.stderr,
        )
    if not artefacts:
        print(
            f"note: no spot package in {package_dir}; the package scenarios will be skipped.",
            file=sys.stderr,
        )
    if not cgroup_v2_present(runner):
        print(
            "note: /sys/fs/cgroup/cgroup.controllers is missing: rootless systemd in a "
            "container needs a cgroup v2 host.",
            file=sys.stderr,
        )

    print(
        f"packaging verify: {len(distro_order)} distro(s), {len(selected)} scenario(s), "
        f"{len(artefacts)} artefact(s) in {package_dir}"
    )
    print(f"runtime: {RUNTIME}")

    driver = Driver(
        runner,
        default_clock(),
        time.sleep,
        runtime=RUNTIME,
        package_dir=package_dir if package_dir.exists() else None,
        binary=binary,
        keep=args.keep,
    )
    try:
        rows = driver.run(distro_order, selected)
    finally:
        driver.close()

    print()
    print(report.render_table(rows))
    if args.report is not None:
        written = report.write(args.report, rows, [distro.name for distro in distro_order])
        print(f"report written to {written}")
    return report.exit_code(rows)


if __name__ == "__main__":
    sys.exit(main())
