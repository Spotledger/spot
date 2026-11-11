"""Tests for tools/security/sbom.py: component collection and CycloneDX output
(WP-PRD-08.T2).

Everything runs against committed fixtures, so neither cargo, a lock file in
tree nor a network is needed: cargo metadata is only exercised through the
parsed shape it produces. Covers: Rust and NuGet collection, hash decoding,
CycloneDX required fields, byte determinism and the timestamp flag.
"""
from __future__ import annotations

import contextlib
import io
import json
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE.parent))

import sbom  # noqa: E402

FIXTURES = HERE / "fixtures"
METADATA_FIXTURE = FIXTURES / "cargo_metadata.json"
LOCK_FIXTURE = FIXTURES / "packages.lock.json"
EXPECTED_SHA512_CONTENT = ("000102030405060708090a0b0c0d0e0f1011121314151617"
                           "18191a1b1c1d1e1f202122232425262728292a2b2c2d2e2f"
                           "303132333435363738393a3b3c3d3e3f")


def metadata() -> dict:
    return json.loads(METADATA_FIXTURE.read_text(encoding="utf-8"))


class RustComponentsTest(unittest.TestCase):
    def setUp(self):
        self.components = sbom.rust_components(metadata())

    def test_workspace_crates_are_applications(self):
        runtime = [c for c in self.components if c["name"] == "spot-runtime"][0]
        self.assertEqual(runtime["type"], "application")
        self.assertEqual(runtime["purl"], "pkg:cargo/spot-runtime@0.1.0")
        self.assertNotIn("licenses", runtime)   # license is null in metadata
        self.assertNotIn("hashes", runtime)     # checksum is null in metadata

    def test_registry_crates_are_libraries_with_licence_and_lock_checksum(self):
        serde = [c for c in self.components if c["name"] == "serde"][0]
        self.assertEqual(serde["type"], "library")
        self.assertEqual(serde["licenses"], [{"license": {"expression": "MIT OR Apache-2.0"}}])
        self.assertEqual(serde["hashes"], [{"alg": "SHA-256", "content":
                                            "b3bcd2f7a3c0c4a1b3bcd2f7a3c0c4a1"
                                            "b3bcd2f7a3c0c4a1b3bcd2f7a3c0c4a1"}])

    def test_path_dependencies_are_libraries_without_a_checksum(self):
        localdep = [c for c in self.components if c["name"] == "localdep"][0]
        self.assertEqual(localdep["type"], "library")
        self.assertNotIn("hashes", localdep)
        self.assertEqual(localdep["licenses"], [{"license": {"expression": "Apache-2.0"}}])

    def test_workspace_members_that_are_missing_are_broken(self):
        with self.assertRaises(sbom.SbomError):
            sbom.rust_components({"workspace_members": [], "packages": [{"name": "x"}]})

class NugetComponentsTest(unittest.TestCase):
    def setUp(self):
        self.components = sbom.nuget_components([LOCK_FIXTURE])

    def test_lock_packages_become_library_components(self):
        avalonia = [c for c in self.components if c["name"] == "Avalonia"][0]
        self.assertEqual(avalonia["type"], "library")
        self.assertEqual(avalonia["version"], "12.1.3")
        self.assertEqual(avalonia["purl"], "pkg:nuget/Avalonia@12.1.3")

    def test_content_hash_decodes_to_sha512(self):
        avalonia = [c for c in self.components if c["name"] == "Avalonia"][0]
        self.assertEqual(avalonia["hashes"], [{"alg": "SHA-512", "content": EXPECTED_SHA512_CONTENT}])

    def test_packages_without_a_hash_omit_it_and_projects_are_dropped(self):
        self.assertFalse(any(c["name"] == "Spotledger.Client" for c in self.components))
        fluent = [c for c in self.components if c["name"] == "FluentAssertions"][0]
        self.assertNotIn("hashes", fluent)

    def test_the_same_package_across_frameworks_merges_to_one_component(self):
        avalonia = [c for c in self.components if c["name"] == "Avalonia"]
        self.assertEqual(len(avalonia), 1)

    def test_unreadable_lock_fails_loudly(self):
        with tempfile.TemporaryDirectory() as tmp:
            broken = Path(tmp) / "packages.lock.json"
            broken.write_text("{not json", encoding="utf-8")
            with self.assertRaises(sbom.SbomError):
                sbom.nuget_components([broken])

    def test_content_hash_variants(self):
        self.assertIsNone(sbom.parse_content_hash(""))
        self.assertIsNone(sbom.parse_content_hash("sha256-not-handled"))
        self.assertIsNone(sbom.parse_content_hash("sha512-not base64!"))
        decoded = sbom.parse_content_hash("sha512-/wA=")
        self.assertEqual(decoded, {"alg": "SHA-512", "content": "ff00"})


class BuildAndValidateTest(unittest.TestCase):
    def setUp(self):
        self.document = sbom.build_sbom(sbom.rust_components(metadata()),
                                        sbom.nuget_components([LOCK_FIXTURE]))

    def test_required_cyclonedx_fields(self):
        self.assertEqual(sbom.validate_sbom(self.document), [])
        self.assertEqual(self.document["bomFormat"], "CycloneDX")
        self.assertEqual(self.document["specVersion"], "1.5")
        self.assertEqual(self.document["version"], 1)
        for component in self.document["components"]:
            for key in ("type", "name", "version", "purl", "bom-ref"):
                self.assertTrue(component[key], f"{key} empty in {component}")

    def test_components_are_sorted_and_typed(self):
        purls = [c["purl"] for c in self.document["components"]]
        self.assertEqual(purls, sorted(purls))
        types = {c["name"]: c["type"] for c in self.document["components"]}
        self.assertEqual(types["spot-runtime"], "application")
        self.assertEqual(types["serde"], "library")
        self.assertEqual(types["Avalonia"], "library")

    def test_no_timestamp_by_default(self):
        self.assertNotIn("timestamp", self.document["metadata"])

    def test_timestamp_only_when_requested(self):
        stamped = sbom.build_sbom([], [], timestamp="2026-10-05T12:00:00Z")
        self.assertEqual(stamped["metadata"]["timestamp"], "2026-10-05T12:00:00Z")

    def test_validate_reports_broken_documents(self):
        broken = {"bomFormat": "SPDX", "specVersion": "9.9", "components": [
            {"name": "x"}, {"bom-ref": "", "purl": "", "type": "library", "name": "y",
                            "version": "1", "hashes": [{"alg": "SHA-256"}],
                            "licenses": [{"license": {}}]}]}
        errors = sbom.validate_sbom(broken)
        self.assertEqual(len(errors), 11)  # 3 document-level, 4 on components[0], 4 on components[1]
        self.assertTrue(any("bomFormat" in e for e in errors))
        self.assertTrue(any("specVersion" in e for e in errors))
        self.assertTrue(any("version must be an integer" == e for e in errors))
        self.assertTrue(any("components[0]: missing 'purl'" in e for e in errors))
        self.assertTrue(any("hashes[0]" in e for e in errors))
        self.assertTrue(any("licenses[0]" in e for e in errors))

class DeterminismTest(unittest.TestCase):
    """The acceptance-critical property: two runs produce identical bytes."""

    def serialise(self, **kwargs) -> bytes:
        document = sbom.build_sbom(sbom.rust_components(metadata()),
                                   sbom.nuget_components([LOCK_FIXTURE]), **kwargs)
        return (json.dumps(document, indent=2, sort_keys=True) + "\n").encode("utf-8")

    def test_two_runs_are_byte_identical(self):
        first, second = self.serialise(), self.serialise()
        self.assertEqual(first, second)
        self.assertNotIn(b"timestamp", first)

    def test_serialised_document_is_valid_stable_json(self):
        document = json.loads(self.serialise().decode("utf-8"))
        self.assertEqual(sbom.validate_sbom(document), [])

    def test_timestamp_changes_the_bytes(self):
        self.assertNotEqual(self.serialise(), self.serialise(timestamp="2026-10-05T12:00:00Z"))


class CliTest(unittest.TestCase):
    """main() wiring against a temporary repository root (no cargo involved)."""

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        (Path(self.tmp.name) / "src").mkdir()
        (Path(self.tmp.name) / "src" / "packages.lock.json").write_text(
            LOCK_FIXTURE.read_text(encoding="utf-8"), encoding="utf-8")

    def test_writes_a_valid_sbom_and_is_rerun_identical(self):
        one, two = Path(self.tmp.name) / "one.json", Path(self.tmp.name) / "two.json"
        stdout = io.StringIO()
        with contextlib.redirect_stdout(stdout):
            self.assertEqual(sbom.main(["--out", str(one), "--root", self.tmp.name]), 0)
            self.assertEqual(sbom.main(["--out", str(two), "--root", self.tmp.name]), 0)
        self.assertEqual(one.read_bytes(), two.read_bytes())
        document = json.loads(one.read_text(encoding="utf-8"))
        self.assertEqual(document["bomFormat"], "CycloneDX")
        names = {c["name"] for c in document["components"]}
        self.assertEqual(names, {"Avalonia", "FluentAssertions"})
        self.assertIn("2 components (0 application, 2 library)", stdout.getvalue())

    def test_missing_cargo_toml_is_reported_and_skipped(self):
        out = Path(self.tmp.name) / "skip.json"
        with contextlib.redirect_stdout(io.StringIO()) as stdout:
            self.assertEqual(sbom.main(["--out", str(out), "--root", self.tmp.name]), 0)
        self.assertIn("no Cargo.toml", stdout.getvalue())

    def test_broken_lock_file_fails_with_exit_2(self):
        (Path(self.tmp.name) / "src" / "packages.lock.json").write_text("{", encoding="utf-8")
        stderr = io.StringIO()
        with contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(stderr):
            self.assertEqual(sbom.main(["--out", str(Path(self.tmp.name) / "x.json"),
                                        "--root", self.tmp.name]), 2)
        self.assertIn("unreadable lock file", stderr.getvalue())


if __name__ == "__main__":
    unittest.main()