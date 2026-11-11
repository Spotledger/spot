"""Scenario: the runtime image from ``packaging/linux/container/Containerfile``.

The image is the same artefact chain as the packages — a prebuilt release binary, no
compiler — so this scenario builds it with the release binary, runs it as the non-root user
the Containerfile declares, with a read-only root filesystem, waits for the image's own
``HEALTHCHECK`` to report healthy, proves the root filesystem really is read-only, and stops
it with ``SIGTERM`` expecting exit code 0 inside the grace period.

It runs once per invocation (:data:`SHARED`), not once per distro: the image is built from
``fedora-minimal`` whatever the host distribution is, so repeating it three times would
prove the same thing three times.
"""

from __future__ import annotations

import shutil
from pathlib import Path

from context import Context, ScenarioSkip, Step
from distros import IMAGE_HEALTH_S, IMAGE_SIGTERM_GRACE_S, PACKAGING_DIR, Distro

NAME = "container_image"
PROVES = (
    "the shipped Containerfile builds from a release binary, the image runs non-root with "
    "a read-only root filesystem, its HEALTHCHECK turns healthy, and SIGTERM ends it with "
    "exit code 0 inside the grace period"
)
REQUIRES = ()
SHARED = True

#: How often the health status is polled (seconds).
HEALTH_POLL_S = 3
#: The container name used for the image run.
CONTAINER_NAME = "spot-pkgverify-image"
#: The version used to tag the image when no artefact names one.
FALLBACK_VERSION = "0.0.0"
#: The build context file names the Containerfile expects.
BINARY_NAME = "spot"
CONFIG_NAME = "spot.toml"


def plan(distro: Distro, package: str = "", newer: str = "") -> tuple[Step, ...]:
    """The host commands this scenario runs, for ``--dry-run``."""
    del distro, package, newer
    return (
        Step(
            "build",
            (
                "podman",
                "build",
                "--format",
                "oci",
                "--build-arg",
                "<VERSION>",
                "-t",
                "<image>",
                "-f",
                "<repo>/packaging/linux/container/Containerfile",
                "<work>/image",
            ),
            "exit 0",
        ),
        Step(
            "run",
            (
                "podman",
                "run",
                "-d",
                "--name",
                CONTAINER_NAME,
                "--read-only",
                "--tmpfs",
                "/run",
                "--tmpfs",
                "/tmp",
                "--tmpfs",
                "/var/lib/spot",
                "<image>",
            ),
            "exit 0; the container is running as the image's non-root user",
        ),
        Step(
            "user",
            ("podman", "exec", CONTAINER_NAME, "id", "-u"),
            "a uid other than 0",
        ),
        Step(
            "image user",
            ("podman", "inspect", "--format", "{{.Config.User}}", CONTAINER_NAME),
            "spot",
        ),
        Step(
            "healthcheck",
            ("podman", "inspect", "--format", "{{.State.Health.Status}}", CONTAINER_NAME),
            f"healthy within {IMAGE_HEALTH_S} s",
        ),
        Step(
            "read-only root",
            ("podman", "exec", CONTAINER_NAME, "touch", "/etc/verify-probe"),
            "a non-zero exit (the root filesystem is read-only)",
        ),
        Step(
            "sigterm",
            ("podman", "stop", "--time", str(IMAGE_SIGTERM_GRACE_S), CONTAINER_NAME),
            "exit 0",
        ),
        Step(
            "exit code",
            ("podman", "inspect", "--format", "{{.State.ExitCode}}", CONTAINER_NAME),
            "0 (SIGTERM handled inside the grace period)",
        ),
        Step("cleanup", ("podman", "rm", "-f", CONTAINER_NAME), "exit 0"),
    )


def _prepare_context(context: Context) -> tuple[Path, str]:
    """Stage the build context and return it with the image tag."""
    if context.binary is None:
        raise ScenarioSkip("no release binary (pass --binary PATH)")
    if context.work_dir is None:
        raise ScenarioSkip("the driver did not create a work directory")

    versions = [artefact.version for artefact in context.artefacts]
    version = versions[-1] if versions else FALLBACK_VERSION

    target = Path(context.work_dir) / "image"
    target.mkdir(parents=True, exist_ok=True)
    binary = target / BINARY_NAME
    shutil.copyfile(context.binary, binary)
    binary.chmod(0o755)
    shutil.copyfile(PACKAGING_DIR / "etc" / "spot.toml", target / CONFIG_NAME)
    return target, f"localhost/spot-pkgverify-image:{version}"


def run(context: Context) -> str:
    """Build, run, probe and stop the image; returns a one-line detail."""
    build_context, image = _prepare_context(context)
    started = False
    try:
        build = context.host(
            [
                context.runtime,
                "build",
                "--format",
                "oci",
                "--build-arg",
                f"VERSION={image.rsplit(':', 1)[-1]}",
                "-t",
                image,
                "-f",
                str(PACKAGING_DIR / "container" / "Containerfile"),
                str(build_context),
            ]
        )
        context.check_ok(build, "podman build failed for the Containerfile")

        run = context.host(
            [
                context.runtime,
                "run",
                "-d",
                "--name",
                CONTAINER_NAME,
                "--read-only",
                "--tmpfs",
                "/run",
                "--tmpfs",
                "/tmp",
                "--tmpfs",
                "/var/lib/spot",
                "--security-opt",
                "label=disable",
                image,
            ]
        )
        context.check_ok(run, "podman run failed for the image")
        started = True

        uid = context.host([context.runtime, "exec", CONTAINER_NAME, "id", "-u"])
        context.check_ok(uid, "the container is not running")
        context.check(
            uid.stdout.strip() not in ("", "0"),
            f"the container runs as root (uid {uid.stdout.strip()!r})",
        )

        configured_user = context.host(
            [context.runtime, "inspect", "--format", "{{.Config.User}}", CONTAINER_NAME]
        )
        context.check(
            "spot" in configured_user.stdout,
            f"the image user is {configured_user.stdout.strip()!r}, expected spot",
        )

        def healthy() -> bool:
            status = context.host(
                [context.runtime, "inspect", "--format", "{{.State.Health.Status}}", CONTAINER_NAME]
            )
            return status.stdout.strip() == "healthy"

        health_after = context.poll_until(
            healthy,
            deadline_s=IMAGE_HEALTH_S,
            interval_s=HEALTH_POLL_S,
            what="the image HEALTHCHECK reporting healthy",
        )

        read_only = context.host(
            [context.runtime, "exec", CONTAINER_NAME, "touch", "/etc/verify-probe"]
        )
        context.check(
            not read_only.ok,
            "the root filesystem is writable inside the container",
        )

        stop = context.host(
            [context.runtime, "stop", "--time", str(IMAGE_SIGTERM_GRACE_S), CONTAINER_NAME]
        )
        context.check_ok(stop, "podman stop failed")
        exit_code = context.host(
            [context.runtime, "inspect", "--format", "{{.State.ExitCode}}", CONTAINER_NAME]
        )
        context.check(
            exit_code.stdout.strip() == "0",
            f"the container exited {exit_code.stdout.strip()!r} on SIGTERM, expected 0",
        )

        return (
            f"{image} built and run as uid {uid.stdout.strip()}; healthy after "
            f"{health_after} s; root filesystem read-only; SIGTERM exit 0"
        )
    finally:
        if started:
            context.host([context.runtime, "rm", "-f", CONTAINER_NAME])
