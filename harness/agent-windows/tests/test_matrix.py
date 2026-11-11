"""Tests for the checked-in matrix definition and its loader (WP-ADM-05.T3)."""

from __future__ import annotations

import dataclasses
import json
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

import orchestrator  # noqa: E402
from orchestrator import MatrixError, load_matrix  # noqa: E402

VALID_MATRIX = """\
schema = 1
module = "spot-agent-windows"
wp = "WP-ADM-05"

[capabilities.admin]
description = "elevated"
probe = "token is elevated"

[capabilities.vss]
description = "shadow copies"
probe = "vssadmin answers"

[[targets]]
name = "windows-server-2022"
os = "Windows Server"
version = "2022"
edition = "Datacenter"
image = "windows-server-2022-eval"
runner_labels = ["Windows", "windows-vm"]
capabilities = ["admin", "vss"]

[[tests]]
name = "services"
file = "tests-real/tests/services.rs"
description = "services"
capabilities = ["admin"]
"""


def write_matrix(text: str) -> Path:
    directory = tempfile.mkdtemp()
    path = Path(directory) / "matrix.toml"
    path.write_text(text, encoding="utf-8")
    return path


class TestShippedMatrix(unittest.TestCase):
    """The matrix that ships in the repository must satisfy the brief."""

    @classmethod
    def setUpClass(cls) -> None:
        cls.matrix = load_matrix()

    def test_header(self) -> None:
        self.assertEqual(self.matrix.schema, orchestrator.SCHEMA)
        self.assertEqual(self.matrix.module, "spot-agent-windows")
        self.assertEqual(self.matrix.wp, "WP-ADM-05")

    def test_every_required_target_is_listed(self) -> None:
        names = {target.name for target in self.matrix.targets}
        self.assertEqual(
            names,
            {
                "windows-server-2022",
                "windows-server-2025",
                "windows-10-22h2",
                "windows-11",
            },
        )

    def test_every_target_offers_every_required_capability(self) -> None:
        for target in self.matrix.targets:
            offered = set(target.capabilities)
            for required in orchestrator.REQUIRED_CAPABILITIES:
                self.assertIn(required, offered, f"{target.name} lacks {required}")
            self.assertTrue(target.runner_labels, target.name)
            self.assertTrue(target.image, target.name)

    def test_every_capability_documents_itself_and_its_probe(self) -> None:
        for capability in self.matrix.capabilities:
            self.assertTrue(capability.description.strip(), capability.name)
            self.assertTrue(capability.probe.strip(), capability.name)
        for required in orchestrator.REQUIRED_CAPABILITIES:
            self.assertIn(required, self.matrix.capability_names)

    def test_tests_are_unique_and_use_known_capabilities(self) -> None:
        names = [test.name for test in self.matrix.tests]
        self.assertEqual(len(names), len(set(names)))
        self.assertTrue(names, "the matrix must list at least one real test")
        for test in self.matrix.tests:
            for capability in test.capabilities:
                self.assertIn(capability, self.matrix.capability_names, test.name)

    def test_every_test_file_exists_and_matches_its_name(self) -> None:
        for test in self.matrix.tests:
            path = self.matrix.test_path(test)
            self.assertTrue(path.is_file(), f"{test.name}: {path} is missing")
            self.assertEqual(path.stem, test.name, f"{test.name}: file stem mismatch")

    def test_test_capabilities_are_a_subset_of_every_target(self) -> None:
        # A test whose capabilities no target offers would always be skipped; that would
        # make the matrix a lie, so it is a hard error here.
        for test in self.matrix.tests:
            for target in self.matrix.targets:
                missing = self.matrix.missing_capabilities(target, test)
                self.assertEqual(missing, (), f"{target.name}/{test.name}: {missing}")

    def test_json_view_is_serialisable(self) -> None:
        view = orchestrator.matrix_to_dict(self.matrix)
        rendered = json.dumps(view, sort_keys=True)
        self.assertIn("windows-11", rendered)
        self.assertEqual(
            {target["name"] for target in view["targets"]},
            {target.name for target in self.matrix.targets},
        )

    def test_missing_capabilities_reports_the_gap(self) -> None:
        target = self.matrix.target("windows-11")
        test = self.matrix.real_test("services")
        self.assertEqual(self.matrix.missing_capabilities(target, test), ())
        hollow = dataclasses.replace(target, capabilities=())
        self.assertEqual(self.matrix.missing_capabilities(hollow, test), ("admin",))

    def test_unknown_names_raise(self) -> None:
        with self.assertRaises(MatrixError):
            self.matrix.target("windows-98")
        with self.assertRaises(MatrixError):
            self.matrix.real_test("nope")
        with self.assertRaises(MatrixError):
            self.matrix.capability("nope")


class TestMatrixValidation(unittest.TestCase):
    """The loader must reject a matrix that cannot be trusted."""

    def test_accepts_a_valid_matrix(self) -> None:
        matrix = load_matrix(write_matrix(VALID_MATRIX))
        self.assertEqual(len(matrix.targets), 1)
        self.assertEqual(len(matrix.tests), 1)

    def test_missing_file(self) -> None:
        with self.assertRaises(MatrixError):
            load_matrix("/nonexistent/matrix.toml")

    def test_invalid_toml(self) -> None:
        with self.assertRaises(MatrixError):
            load_matrix(write_matrix("this is not toml ="))

    def test_wrong_schema(self) -> None:
        with self.assertRaises(MatrixError):
            load_matrix(write_matrix(VALID_MATRIX.replace("schema = 1", "schema = 2")))

    def test_duplicate_target_name(self) -> None:
        doubled = VALID_MATRIX + """
[[targets]]
name = "windows-server-2022"
os = "Windows Server"
version = "2022"
edition = "Datacenter"
image = "windows-server-2022-eval"
runner_labels = ["Windows"]
capabilities = ["admin"]
"""
        with self.assertRaises(MatrixError):
            load_matrix(write_matrix(doubled))

    def test_unknown_capability_reference(self) -> None:
        broken = VALID_MATRIX.replace('capabilities = ["admin", "vss"]', 'capabilities = ["admin", "ghost"]')
        with self.assertRaises(MatrixError):
            load_matrix(write_matrix(broken))

    def test_missing_required_key(self) -> None:
        broken = VALID_MATRIX.replace('probe = "token is elevated"\n', "")
        with self.assertRaises(MatrixError):
            load_matrix(write_matrix(broken))

    def test_empty_target_list(self) -> None:
        broken = VALID_MATRIX.split("[[targets]]")[0]
        with self.assertRaises(MatrixError):
            load_matrix(write_matrix(broken))


if __name__ == "__main__":
    unittest.main()
