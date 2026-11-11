"""Read and validate ``matrix.toml``: the checked-in target and test matrix.

The matrix is data, so this module is the only place that knows its schema. Everything
downstream works with the frozen dataclasses here, and a malformed matrix fails loudly at
load time rather than halfway through a container run. Standard library only (``tomllib``,
Python 3.11+).
"""

from __future__ import annotations

import tomllib
from dataclasses import dataclass
from pathlib import Path

#: The capability vocabulary. A target gives some of these; a test needs some of them.
#: A name outside this set is a typo and is refused, so a matrix cannot silently claim a
#: capability the orchestrator does not know how to check.
CAPABILITIES = frozenset(
    {
        "systemd",
        "privileged",
        "packages",
        "network",
        "btrfs_loop",
        "lvm_loop",
        "sysctl",
        "chrony",
        "selinux",
        "apparmor",
        "firewalld",
        "ufw",
    }
)

#: How a target's image is obtained.
IMAGE_SOURCES = frozenset({"pull", "build", "qemu"})

#: The run modes: containers (the default) or qemu VMs.
MODES = frozenset({"container", "vm"})


class MatrixError(ValueError):
    """A malformed matrix file."""


@dataclass(frozen=True)
class Target:
    """One distribution to run the suite against."""

    name: str
    family: str
    image: str
    image_source: str = "pull"
    dockerfile: str | None = None
    mode: str = "container"
    capabilities: frozenset[str] = frozenset()

    def gives(self, capability: str) -> bool:
        """Whether this target gives ``capability``."""
        return capability in self.capabilities

    @property
    def missing(self) -> frozenset[str]:
        """Capabilities a real target in this mode could still be missing (informational)."""
        return CAPABILITIES - self.capabilities


@dataclass(frozen=True)
class TestCase:
    """One real-system integration test and the capabilities it needs."""

    name: str
    requires: frozenset[str]
    vm_only: bool
    description: str = ""


@dataclass(frozen=True)
class Matrix:
    """The whole matrix: its targets, its tests and its defaults."""

    version: int
    junit_dir: str
    targets: tuple[Target, ...]
    tests: tuple[TestCase, ...]

    def target(self, name: str) -> Target:
        """The target named ``name``.

        Raises:
            KeyError: when no target has that name.
        """
        for target in self.targets:
            if target.name == name:
                return target
        raise KeyError(name)

    def test(self, name: str) -> TestCase:
        """The test named ``name``.

        Raises:
            KeyError: when no test has that name.
        """
        for test in self.tests:
            if test.name == name:
                return test
        raise KeyError(name)


def _require(table: dict, key: str, where: str):
    if key not in table:
        raise MatrixError(f"{where}: missing required key {key!r}")
    return table[key]


def _string_list(value, where: str) -> frozenset[str]:
    if not isinstance(value, list) or not all(isinstance(item, str) for item in value):
        raise MatrixError(f"{where}: expected a list of strings")
    unknown = sorted(set(value) - CAPABILITIES)
    if unknown:
        raise MatrixError(
            f"{where}: unknown capabilities {unknown}; "
            f"the vocabulary is {sorted(CAPABILITIES)}"
        )
    return frozenset(value)


def _parse_target(table: dict) -> Target:
    where = "[[target]]"
    name = _require(table, "name", where)
    if not isinstance(name, str) or not name:
        raise MatrixError(f"{where}: 'name' must be a non-empty string")
    where = f"[[target]] {name}"
    family = _require(table, "family", where)
    image = _require(table, "image", where)
    image_source = table.get("image_source", "pull")
    if image_source not in IMAGE_SOURCES:
        raise MatrixError(
            f"{where}: image_source {image_source!r} is not one of {sorted(IMAGE_SOURCES)}"
        )
    mode = table.get("mode", "container")
    if mode not in MODES:
        raise MatrixError(f"{where}: mode {mode!r} is not one of {sorted(MODES)}")
    dockerfile = table.get("dockerfile")
    if image_source == "build" and not dockerfile:
        raise MatrixError(f"{where}: image_source 'build' needs a 'dockerfile'")
    capabilities = _string_list(table.get("capabilities", []), where)
    return Target(
        name=name,
        family=family,
        image=image,
        image_source=image_source,
        dockerfile=dockerfile,
        mode=mode,
        capabilities=capabilities,
    )


def _parse_test(table: dict) -> TestCase:
    where = "[[test]]"
    name = _require(table, "name", where)
    if not isinstance(name, str) or not name:
        raise MatrixError(f"{where}: 'name' must be a non-empty string")
    where = f"[[test]] {name}"
    requires = _string_list(table.get("requires", []), where)
    vm_only = table.get("vm_only", False)
    if not isinstance(vm_only, bool):
        raise MatrixError(f"{where}: 'vm_only' must be a boolean")
    description = table.get("description", "")
    if not isinstance(description, str):
        raise MatrixError(f"{where}: 'description' must be a string")
    return TestCase(
        name=name,
        requires=requires,
        vm_only=vm_only,
        description=description,
    )


def load_matrix(path: str | Path) -> Matrix:
    """Load and validate the matrix at ``path``.

    Raises:
        MatrixError: when the file is missing, unreadable or malformed.
    """
    path = Path(path)
    try:
        raw = path.read_bytes()
    except OSError as error:
        raise MatrixError(f"cannot read the matrix at {path}: {error}") from error
    try:
        document = tomllib.loads(raw.decode("utf-8"))
    except (tomllib.TOMLDecodeError, UnicodeDecodeError) as error:
        raise MatrixError(f"{path} is not valid TOML: {error}") from error

    version = document.get("version")
    if version != 1:
        raise MatrixError(f"{path}: version must be 1, got {version!r}")

    defaults = document.get("defaults", {})
    if not isinstance(defaults, dict):
        raise MatrixError(f"{path}: [defaults] must be a table")
    junit_dir = defaults.get("junit_dir", "junit")
    if not isinstance(junit_dir, str) or not junit_dir:
        raise MatrixError(f"{path}: [defaults] junit_dir must be a non-empty string")

    raw_targets = document.get("target", [])
    raw_tests = document.get("test", [])
    if not isinstance(raw_targets, list) or not raw_targets:
        raise MatrixError(f"{path}: at least one [[target]] is required")
    if not isinstance(raw_tests, list) or not raw_tests:
        raise MatrixError(f"{path}: at least one [[test]] is required")

    targets = tuple(_parse_target(entry) for entry in raw_targets)
    tests = tuple(_parse_test(entry) for entry in raw_tests)

    names = [target.name for target in targets]
    duplicates = sorted({name for name in names if names.count(name) > 1})
    if duplicates:
        raise MatrixError(f"{path}: duplicate target names {duplicates}")
    test_names = [test.name for test in tests]
    duplicates = sorted({name for name in test_names if test_names.count(name) > 1})
    if duplicates:
        raise MatrixError(f"{path}: duplicate test names {duplicates}")

    return Matrix(
        version=version,
        junit_dir=junit_dir,
        targets=targets,
        tests=tests,
    )
