"""Fixtures shared by the packaging-verify tests: no podman, no root, no network.

Every test drives the driver's seams — a :class:`~runtime.StubRunner` with scripted
responses and a ticking clock — or the command line as a subprocess with a stripped ``PATH``.
Nothing here starts a container, and nothing here needs a package built.
"""

import os
import subprocess
import sys
import tempfile
from pathlib import Path

VERIFY_DIR = Path(__file__).resolve().parents[1]
TESTS_DIR = Path(__file__).resolve().parent
RUN_PY = VERIFY_DIR / "run.py"
REPO_ROOT = VERIFY_DIR.parents[2]

for _path in (str(VERIFY_DIR), str(TESTS_DIR)):
    if _path not in sys.path:
        sys.path.insert(0, _path)

import packages  # noqa: E402
from context import Context  # noqa: E402
from distros import FEDORA_44, UBUNTU_2404  # noqa: E402
from runtime import StubRunner  # noqa: E402

RPM_OLD = "spot-1.2.3-1.fc44.x86_64.rpm"
RPM_NEW = "spot-1.2.4-1.fc44.x86_64.rpm"
DEB_OLD = "spot_1.2.3_amd64.deb"
DEB_NEW = "spot_1.2.4_amd64.deb"


class TickingClock:
    """A clock that advances a fixed step per reading, so a poll loop terminates."""

    def __init__(self, step_ms: int = 1000, start_ms: int = 0) -> None:
        self.step_ms = step_ms
        self.value = start_ms

    def now_ms(self) -> int:
        current = self.value
        self.value += self.step_ms
        return current


def rpm(version: str, release: str = "1", directory: Path = Path("/pkgs"),
        filename: str | None = None) -> packages.Artefact:
    """An RPM artefact descriptor; the file does not have to exist."""
    name = filename or f"spot-{version}-{release}.fc44.x86_64.rpm"
    return packages.Artefact(
        path=directory / name,
        package_format="rpm",
        name="spot",
        version=version,
        release=release,
        arch="x86_64",
    )


def deb(version: str, directory: Path = Path("/pkgs"),
        filename: str | None = None) -> packages.Artefact:
    """A DEB artefact descriptor; the file does not have to exist."""
    name = filename or f"spot_{version}_amd64.deb"
    return packages.Artefact(
        path=directory / name,
        package_format="deb",
        name="spot",
        version=version,
        arch="amd64",
    )


def context_for(
    distro=FEDORA_44,
    artefacts: tuple = (),
    *,
    work_dir: Path | None = None,
    binary: Path | None = None,
    clock=None,
    runner: StubRunner | None = None,
    sleeps: list[int] | None = None,
) -> tuple[Context, StubRunner, list[int]]:
    """A context wired to a stub runner and a ticking clock, plus the recorded sleeps."""
    stub = runner or StubRunner()
    recorded: list[int] = sleeps if sleeps is not None else []
    context = Context(
        distro=distro,
        runner=stub,
        clock=clock or TickingClock(),
        sleep=recorded.append,
        runtime="podman",
        container=distro.container_name,
        artefacts=tuple(artefacts),
        package_dir=Path("/pkgs"),
        binary=binary,
        work_dir=work_dir or Path(tempfile.mkdtemp(prefix="spot-verify-test-")),
    )
    return context, stub, recorded


def write_packages(directory: Path, names: list[str]) -> Path:
    """Create empty files with the given names, to exercise artefact discovery."""
    directory.mkdir(parents=True, exist_ok=True)
    for name in names:
        (directory / name).write_bytes(b"")
    return directory


def run_cli(args: list[str], *, env: dict | None = None) -> subprocess.CompletedProcess:
    """Run ``run.py`` as a subprocess from the repository root."""
    return subprocess.run(
        [sys.executable, str(RUN_PY), *args],
        capture_output=True,
        text=True,
        cwd=str(REPO_ROOT),
        env=env if env is not None else dict(os.environ),
        timeout=300,
        check=False,
    )


def environment_without_podman() -> dict:
    """An environment whose ``PATH`` cannot see podman, docker or anything else."""
    empty = tempfile.mkdtemp(prefix="spot-verify-empty-path-")
    env = dict(os.environ)
    env["PATH"] = empty
    return env


def ubuntu():
    """The Debian-family distro, for symmetry in the scenario tests."""
    return UBUNTU_2404
