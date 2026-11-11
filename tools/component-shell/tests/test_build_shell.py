"""The build tool end to end: it verifies the set, refuses what it must, and writes a shell.

The generated project is not compiled here: it needs the Avalonia browser workload and a browser-side
host, neither of which this task owns. What is checked is everything the tool itself promises — the
packages it bundled are the ones that verified, the references and the component list name them, and
the manifest it wrote is the document the C# host parses.

Every build here runs in the default mode, against root-signed trust material the test generates, and
the set file carries no trust table at all: a tenant set file is not a trust source.
"""

from __future__ import annotations

import json
import subprocess
import sys
import unittest
from pathlib import Path

import support
from component_shell import manifest as set_manifest

TOOL = support.REPOSITORY_ROOT / "tools" / "component-shell" / "build-shell.py"


def run_tool(*arguments: str) -> subprocess.CompletedProcess[str]:
    """Runs the CLI the way a build would, from the repository root."""
    return subprocess.run(
        [sys.executable, str(TOOL), *arguments],
        cwd=support.REPOSITORY_ROOT,
        capture_output=True,
        text=True,
        timeout=300,
        check=False,
    )


def trusted(root: Path, *, publisher_seed: bytes = support.PUBLISHER_SEED) -> support.TrustFiles:
    """Writes this build's root key, anchor set and revocation list, and returns the arguments."""
    return support.write_trust(root / "trust", publisher_seed=publisher_seed)


class BuildShellTests(unittest.TestCase):
    """The command line, its refusals and what it writes."""

    def test_a_verified_set_builds_a_shell(self) -> None:
        with support.TempDirectory() as root:
            package = root / "acme.rating-1.0.0"
            support.build_package(package)
            set_file = support.write_set_file(root / "set.toml", packages=[package], trust_table=False)
            output = root / "publish"
            files = trusted(root)

            completed = run_tool(*files.arguments(), "--components", str(set_file), "--output", str(output))

            self.assertEqual(0, completed.returncode, completed.stderr)
            self.assertIn("1 component(s)", completed.stdout)

            project = (output / "Shell.csproj").read_text(encoding="utf-8")
            self.assertIn("<Reference Include=\"Acme.RatingComponent\">", project)
            self.assertIn("Spotledger.Component.Host.csproj", project)
            self.assertIn("components/acme.rating/1.0.0/assemblies/Acme.RatingComponent.dll", project)

            components = (output / "Components.g.cs").read_text(encoding="utf-8")
            self.assertIn('new("acme.rating", "1.0.0"', components)

            manifests = list(output.glob("component-set-*.json"))
            self.assertEqual(1, len(manifests))
            parsed = set_manifest.parse(manifests[0].read_text(encoding="utf-8"))
            self.assertEqual("test-shell", parsed.shell)
            self.assertEqual(manifests[0].name, parsed.file_name)
            self.assertEqual("acme.rating", parsed.entries[0].component_id)

            # The payload is the bytes that were signed, in the package's own layout.
            bundled = output / "components" / "acme.rating" / "1.0.0" / "assemblies" / "Acme.RatingComponent.dll"
            self.assertEqual(support.hash_bytes(bundled.read_bytes()), parsed.entries[0].entry_assembly_sha256)

            program = (output / "Program.cs").read_text(encoding="utf-8")
            self.assertIn(parsed.file_name, program)
            self.assertIn("ComponentSetManifestReader.TryRead", program)
            self.assertIn("the loaded component list equals the manifest", program)

    def test_an_unsigned_package_is_refused_and_nothing_is_written(self) -> None:
        with support.TempDirectory() as root:
            package = root / "acme.rating-1.0.0"
            support.build_package(package, write_signature=False)
            set_file = support.write_set_file(root / "set.toml", packages=[package], trust_table=False)
            output = root / "publish"
            files = trusted(root)

            completed = run_tool(*files.arguments(), "--components", str(set_file), "--output", str(output))

            self.assertEqual(1, completed.returncode)
            self.assertIn("component.package.signature_missing", completed.stderr)
            self.assertFalse(output.exists())

    def test_a_tampered_package_is_refused(self) -> None:
        with support.TempDirectory() as root:
            package = root / "acme.rating-1.0.0"
            support.build_package(package)
            (package / "assemblies" / "Acme.RatingComponent.dll").write_bytes(b"not the signed bytes")
            set_file = support.write_set_file(root / "set.toml", packages=[package], trust_table=False)
            files = trusted(root)

            completed = run_tool(*files.arguments(), "--components", str(set_file), "--output", str(root / "publish"))

            self.assertEqual(1, completed.returncode)
            self.assertIn("component.package.hash_mismatch", completed.stderr)

    def test_a_key_the_store_does_not_name_is_refused(self) -> None:
        with support.TempDirectory() as root:
            package = root / "acme.rating-1.0.0"
            support.build_package(package, seed=support.OUTSIDER_SEED)
            set_file = support.write_set_file(root / "set.toml", packages=[package], trust_table=False)
            files = trusted(root)

            completed = run_tool(*files.arguments(), "--components", str(set_file), "--output", str(root / "publish"))

            self.assertEqual(1, completed.returncode)
            self.assertIn("component.signature.unknown_key", completed.stderr)

    def test_a_revoked_publisher_is_refused(self) -> None:
        with support.TempDirectory() as root:
            package = root / "acme.rating-1.0.0"
            support.build_package(package)
            set_file = support.write_set_file(root / "set.toml", packages=[package], trust_table=False)
            root_seed = support.generated_seed()
            files = support.write_trust(
                root / "trust",
                root_seed=root_seed,
                revocations=support.revocation_document(
                    root_seed=root_seed, publishers=[support.revoked_publisher("acme")]
                ),
            )

            completed = run_tool(*files.arguments(), "--components", str(set_file), "--output", str(root / "publish"))

            self.assertEqual(1, completed.returncode)
            self.assertIn("component.publisher.revoked", completed.stderr)

    def test_a_non_empty_folder_this_tool_did_not_write_is_not_deleted(self) -> None:
        with support.TempDirectory() as root:
            package = root / "acme.rating-1.0.0"
            support.build_package(package)
            set_file = support.write_set_file(root / "set.toml", packages=[package], trust_table=False)
            output = root / "precious"
            output.mkdir()
            (output / "notes.txt").write_text("keep me", encoding="utf-8")
            files = trusted(root)

            completed = run_tool(*files.arguments(), "--components", str(set_file), "--output", str(output))

            self.assertEqual(1, completed.returncode)
            self.assertEqual("keep me", (output / "notes.txt").read_text(encoding="utf-8"))

    def test_a_folder_this_tool_wrote_is_rebuilt(self) -> None:
        with support.TempDirectory() as root:
            package = root / "acme.rating-1.0.0"
            support.build_package(package)
            set_file = support.write_set_file(root / "set.toml", packages=[package], trust_table=False)
            output = root / "publish"
            files = trusted(root)

            self.assertEqual(
                0, run_tool(*files.arguments(), "--components", str(set_file), "--output", str(output)).returncode
            )
            self.assertEqual(
                0, run_tool(*files.arguments(), "--components", str(set_file), "--output", str(output)).returncode
            )

    def test_a_shell_name_that_could_inject_into_the_project_is_refused(self) -> None:
        with support.TempDirectory() as root:
            package = root / "acme.rating-1.0.0"
            support.build_package(package)
            set_file = support.write_set_file(
                root / "set.toml", packages=[package], name="x</AssemblyName><Exec/>", trust_table=False
            )
            output = root / "publish"
            files = trusted(root)

            completed = run_tool(*files.arguments(), "--components", str(set_file), "--output", str(output))

            self.assertNotEqual(0, completed.returncode)
            self.assertFalse(output.exists())

    def test_the_same_component_twice_is_refused(self) -> None:
        with support.TempDirectory() as root:
            first = root / "one"
            second = root / "two"
            support.build_package(first)
            support.build_package(second)
            set_file = support.write_set_file(root / "set.toml", packages=[first, second], trust_table=False)
            files = trusted(root)

            completed = run_tool(*files.arguments(), "--components", str(set_file), "--output", str(root / "publish"))

            self.assertEqual(1, completed.returncode)
            self.assertIn("component.set.duplicate", completed.stderr)

    def test_a_set_file_that_is_not_toml_is_refused(self) -> None:
        with support.TempDirectory() as root:
            set_file = root / "set.toml"
            set_file.write_text("this is not toml", encoding="utf-8")
            files = trusted(root)

            completed = run_tool(*files.arguments(), "--components", str(set_file))

            self.assertNotEqual(0, completed.returncode)

    def test_a_set_file_without_a_shell_name_is_refused(self) -> None:
        with support.TempDirectory() as root:
            set_file = root / "set.toml"
            set_file.write_text('[[component]]\npackage = "x"\n', encoding="utf-8")
            files = trusted(root)

            completed = run_tool(*files.arguments(), "--components", str(set_file))

            self.assertNotEqual(0, completed.returncode)
            self.assertIn("[shell]", completed.stderr)

    def test_a_set_file_that_names_trust_keys_is_refused(self) -> None:
        with support.TempDirectory() as root:
            package = root / "acme.rating-1.0.0"
            support.build_package(package)
            set_file = support.write_set_file(root / "set.toml", packages=[package])
            files = trusted(root)

            completed = run_tool(*files.arguments(), "--components", str(set_file), "--output", str(root / "publish"))

            self.assertEqual(1, completed.returncode)
            self.assertIn("component.trust.set_carries_keys", completed.stderr)

    def test_two_components_get_two_references_and_one_manifest_entry_each(self) -> None:
        with support.TempDirectory() as root:
            first = root / "acme.rating-1.0.0"
            second = root / "acme.badge-1.0.0"
            support.build_package(first)
            support.build_package(
                second,
                component_id="acme.badge",
                assembly_path="assemblies/Acme.Badge.dll",
                assembly=b"MZ badge",
            )
            set_file = support.write_set_file(root / "set.toml", packages=[second, first], trust_table=False)
            output = root / "publish"
            files = trusted(root)

            completed = run_tool(*files.arguments(), "--components", str(set_file), "--output", str(output))

            self.assertEqual(0, completed.returncode, completed.stderr)
            manifest_path = next(output.glob("component-set-*.json"))
            parsed = set_manifest.parse(manifest_path.read_text(encoding="utf-8"))
            self.assertEqual(["acme.badge", "acme.rating"], [entry.component_id for entry in parsed.entries])
            self.assertIn("Acme.Badge", (output / "Shell.csproj").read_text(encoding="utf-8"))

    def test_the_printed_manifest_is_the_document_it_wrote(self) -> None:
        with support.TempDirectory() as root:
            package = root / "acme.rating-1.0.0"
            support.build_package(package)
            set_file = support.write_set_file(root / "set.toml", packages=[package], trust_table=False)
            output = root / "publish"
            files = trusted(root)

            completed = run_tool(
                *files.arguments(), "--components", str(set_file), "--output", str(output), "--print-manifest"
            )

            self.assertEqual(0, completed.returncode, completed.stderr)
            printed = completed.stdout.splitlines()
            manifest_path = next(output.glob("component-set-*.json"))
            self.assertEqual(manifest_path.read_text(encoding="utf-8"), printed[0])
            self.assertEqual("acme.rating", json.loads(printed[0])["components"][0]["component_id"])
            self.assertEqual(set_manifest.parse(printed[0]).file_name, manifest_path.name)


if __name__ == "__main__":
    unittest.main()
