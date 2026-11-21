"""Tests for the matrix loader and the checked-in matrix itself."""

import sys
import tempfile
import unittest
from pathlib import Path

HARNESS_DIR = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(HARNESS_DIR))

MATRIX_PATH = HARNESS_DIR / "matrix.toml"
REAL_CRATE_DIR = HARNESS_DIR / "tests-real"

from agent_linux_harness.matrix import CAPABILITIES, MatrixError, load_matrix  # noqa: E402

MINIMAL = """
version = 1
[defaults]
junit_dir = "junit"

[[target]]
name = "fedora-44"
family = "fedora"
image = "registry.fedoraproject.org/fedora:44"
capabilities = ["systemd", "privileged"]

[[test]]
name = "distro_family_matches_target"
requires = []
vm_only = false
"""


def load_text(text: str):
    with tempfile.TemporaryDirectory() as directory:
        path = Path(directory) / "matrix.toml"
        path.write_text(text, encoding="utf-8")
        return load_matrix(path)


class CheckedInMatrixTests(unittest.TestCase):
    """The real matrix must stay well-formed and cover the required distributions."""

    def setUp(self):
        self.matrix = load_matrix(MATRIX_PATH)

    def test_it_loads(self):
        self.assertEqual(self.matrix.version, 1)
        self.assertTrue(self.matrix.targets)
        self.assertTrue(self.matrix.tests)

    def test_it_has_the_three_container_families(self):
        container = [t for t in self.matrix.targets if t.mode == "container"]
        families = {t.family for t in container}
        self.assertEqual(families, {"fedora", "rhel", "ubuntu"})
        self.assertEqual(len(container), 3)

    def test_it_has_a_vm_target_per_family(self):
        vm = [t for t in self.matrix.targets if t.mode == "vm"]
        self.assertEqual({t.family for t in vm}, {"fedora", "rhel", "ubuntu"})

    def test_firewall_and_mac_tests_are_vm_only(self):
        for test in self.matrix.tests:
            if test.requires & {"firewalld", "ufw", "selinux", "apparmor"}:
                self.assertTrue(test.vm_only, f"{test.name} needs an LSM/firewall but is not vm_only")

    def test_vm_only_capabilities_are_never_on_a_container(self):
        vm_only_caps = {"firewalld", "ufw", "selinux", "apparmor"}
        for target in self.matrix.targets:
            if target.mode == "container":
                self.assertEqual(target.capabilities & vm_only_caps, frozenset())

    def test_every_test_names_a_function_in_the_real_crate(self):
        source = (REAL_CRATE_DIR / "tests" / "real.rs").read_text(encoding="utf-8")
        for test in self.matrix.tests:
            self.assertIn(
                f"fn {test.name}(",
                source,
                f"matrix test {test.name} has no #[test] fn in tests-real/tests/real.rs",
            )

    def test_every_test_is_marked_ignored_in_the_real_crate(self):
        source = (REAL_CRATE_DIR / "tests" / "real.rs").read_text(encoding="utf-8")
        # Each real test must carry #[ignore] so a plain `cargo test` on a host is safe.
        self.assertGreaterEqual(source.count("#[ignore"), len(self.matrix.tests))

    def test_capabilities_are_known(self):
        for target in self.matrix.targets:
            self.assertLessEqual(target.capabilities, CAPABILITIES)
        for test in self.matrix.tests:
            self.assertLessEqual(test.requires, CAPABILITIES)


class LoaderValidationTests(unittest.TestCase):
    """A malformed matrix must fail at load time, not mid-run."""

    def test_minimal_matrix_loads(self):
        matrix = load_text(MINIMAL)
        self.assertEqual(matrix.targets[0].name, "fedora-44")
        self.assertEqual(matrix.tests[0].name, "distro_family_matches_target")

    def test_unknown_capability_is_refused(self):
        bad = MINIMAL.replace('"systemd", "privileged"', '"systemd", "warp_drive"')
        with self.assertRaises(MatrixError):
            load_text(bad)

    def test_unknown_test_capability_is_refused(self):
        bad = MINIMAL.replace("requires = []", 'requires = ["nonsense"]')
        with self.assertRaises(MatrixError):
            load_text(bad)

    def test_duplicate_target_name_is_refused(self):
        bad = MINIMAL + MINIMAL.split("[[target]]", 1)[1]
        with self.assertRaises(MatrixError):
            load_text(bad)

    def test_duplicate_test_name_is_refused(self):
        bad = MINIMAL + MINIMAL.split("[[test]]", 1)[1]
        with self.assertRaises(MatrixError):
            load_text(bad)

    def test_wrong_version_is_refused(self):
        with self.assertRaises(MatrixError):
            load_text(MINIMAL.replace("version = 1", "version = 2"))

    def test_build_source_needs_a_dockerfile(self):
        bad = MINIMAL.replace(
            'image = "registry.fedoraproject.org/fedora:44"',
            'image = "spot/agent-linux-fedora"\nimage_source = "build"',
        )
        with self.assertRaises(MatrixError):
            load_text(bad)

    def test_missing_file_is_refused(self):
        with self.assertRaises(MatrixError):
            load_matrix("/nonexistent/matrix.toml")


if __name__ == "__main__":
    unittest.main()
