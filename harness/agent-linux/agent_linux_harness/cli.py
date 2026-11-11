"""The command line: ``python -m agent_linux_harness.cli`` (``run.sh`` wraps this).

Options cover the three things a person needs: see what the matrix says (``--list``), see
what would run without running it (``--dry-run``), and run it for real. Running it for
real requires the safety gate (``SPOT_HARNESS_DISPOSABLE=1`` and a container/VM root), a
container runtime, and a built test binary (which this builds when not supplied).
Standard library only.
"""

from __future__ import annotations

import argparse
import os
import sys
from pathlib import Path

from .junit import render_junit
from .matrix import Matrix, MatrixError, load_matrix
from .orchestrator import (
    RunPlan,
    build_test_binary,
    check_disposable,
    detect_runtime,
    execute,
    has_failures,
    plan,
    render_table,
)
from .runner import Runner, SubprocessRunner

#: The package's own directory; ``matrix.toml`` and ``tests-real/`` live one level up.
HERE = Path(__file__).resolve().parent
HARNESS_DIR = HERE.parent


def default_matrix_path() -> Path:
    """The checked-in matrix beside this package."""
    return HARNESS_DIR / "matrix.toml"


def build_parser() -> argparse.ArgumentParser:
    """The argument parser."""
    parser = argparse.ArgumentParser(
        prog="agent-linux-harness",
        description="Run the spot-agent-linux real-system suite across the distro matrix.",
    )
    parser.add_argument(
        "--matrix",
        type=Path,
        default=default_matrix_path(),
        help="the matrix TOML (default: %(default)s)",
    )
    parser.add_argument(
        "--mode",
        choices=("container", "vm"),
        default="container",
        help="run the container targets or the qemu VM targets (default: %(default)s)",
    )
    parser.add_argument(
        "--target",
        action="append",
        default=[],
        metavar="NAME",
        help="only this target (repeatable); default is every target in the mode",
    )
    parser.add_argument(
        "--runtime",
        choices=("auto", "podman", "docker"),
        default="auto",
        help="the container runtime; auto prefers podman (default: %(default)s)",
    )
    parser.add_argument(
        "--binary",
        type=Path,
        default=None,
        help="the prebuilt spot-agent-linux-real test binary; built when omitted",
    )
    parser.add_argument(
        "--crate-dir",
        type=Path,
        default=HARNESS_DIR / "tests-real",
        help="the real test crate to build (default: %(default)s)",
    )
    parser.add_argument(
        "--junit-dir",
        type=Path,
        default=None,
        help="where to write JUnit XML (default: <matrix dir>/<junit_dir>)",
    )
    parser.add_argument(
        "--dry-run",
        action="store_true",
        help="print what would run and exit without touching a runtime",
    )
    parser.add_argument(
        "--list",
        action="store_true",
        help="list the targets and tests and exit",
    )
    parser.add_argument(
        "--no-image",
        action="store_true",
        help="skip pulling/building the image (assume it is present)",
    )
    return parser


def describe_plan(run_plan: RunPlan) -> str:
    """A human-readable plan: per target, what runs and what is skipped and why."""
    lines = []
    for target_plan in run_plan.targets:
        lines.append(f"{target_plan.target.name} ({target_plan.target.image})")
        for decision in target_plan.decisions:
            if decision.run:
                lines.append(f"  RUN   {decision.test.name}")
            else:
                lines.append(f"  SKIP  {decision.test.name}  ({decision.reason})")
        lines.append("")
    lines.append(f"{run_plan.runnable_count} test run(s) planned across {len(run_plan.targets)} target(s)")
    return "\n".join(lines)


def describe_matrix(matrix: Matrix) -> str:
    """List the matrix's targets and tests."""
    lines = ["targets:"]
    for target in matrix.targets:
        caps = ", ".join(sorted(target.capabilities)) or "none"
        lines.append(f"  {target.name:20} mode={target.mode:9} family={target.family:8} [{caps}]")
    lines.append("")
    lines.append("tests:")
    for test in matrix.tests:
        needs = ", ".join(sorted(test.requires)) or "nothing"
        vm = " (vm-only)" if test.vm_only else ""
        lines.append(f"  {test.name:40} needs: {needs}{vm}")
    return "\n".join(lines)


def run(args: argparse.Namespace, runner: Runner) -> int:
    """Run the harness for parsed ``args`` over ``runner``; returns an exit code."""
    try:
        matrix = load_matrix(args.matrix)
    except MatrixError as error:
        print(f"error: {error}", file=sys.stderr)
        return 2

    if args.list:
        print(describe_matrix(matrix))
        return 0

    try:
        run_plan = plan(matrix, args.target, args.mode)
    except KeyError as error:
        print(f"error: {error}", file=sys.stderr)
        return 2

    print(describe_plan(run_plan))

    if args.dry_run:
        print("(dry run: nothing was executed)")
        return 0

    if args.mode == "vm":
        print(
            "error: --mode vm is planned and shown here, but the VMs are run by vm/run.sh "
            "(it boots qemu cloud images). Use --dry-run to inspect the plan, or run "
            "./vm/run.sh to execute it.",
            file=sys.stderr,
        )
        return 2

    ok, kind = check_disposable(dict(os.environ), runner)
    if not ok:
        print(
            f"error: refusing to run on this host: {kind}.\n"
            "Set SPOT_HARNESS_DISPOSABLE=1 and run inside a container or a VM "
            "(see README.md). The harness never runs on the host OS.",
            file=sys.stderr,
        )
        return 2

    try:
        runtime = detect_runtime(runner, args.runtime)
    except RuntimeError as error:
        print(f"error: {error}", file=sys.stderr)
        return 2
    print(f"runtime: {runtime} (root is a {kind})")

    binary = args.binary
    if binary is None:
        try:
            binary = build_test_binary(runner, str(args.crate_dir))
        except RuntimeError as error:
            print(f"error: {error}", file=sys.stderr)
            return 2
        print(f"built test binary: {binary}")

    reports = execute(
        run_plan,
        runner,
        runtime=runtime,
        binary=str(binary),
        matrix_dir=str(Path(args.matrix).resolve().parent),
        build_images=not args.no_image,
    )

    junit_dir = args.junit_dir or (Path(args.matrix).resolve().parent / matrix.junit_dir)
    junit_dir.mkdir(parents=True, exist_ok=True)
    for report in reports:
        xml = render_junit([report])
        (junit_dir / f"{report.target}.xml").write_text(xml, encoding="utf-8")
    print(f"JUnit XML written to {junit_dir}")

    print()
    print(render_table(run_plan, reports))

    return 1 if has_failures(reports) else 0


def main(argv: list[str] | None = None) -> int:
    """The entry point."""
    args = build_parser().parse_args(argv)
    return run(args, SubprocessRunner())


if __name__ == "__main__":
    sys.exit(main())
