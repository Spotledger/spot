"""Reading, parsing and verifying a package, and the manifest of a set of them."""

from __future__ import annotations

import json
import unittest

import support
from component_shell import manifest as set_manifest
from component_shell.package import (
    FILE_MISSING,
    HASH_MISMATCH,
    KEY_REVOKED,
    MANIFEST_INVALID,
    MANIFEST_MISSING,
    PUBLISHER_MISMATCH,
    PUBLISHER_REVOKED,
    SIGNATURE_INVALID,
    SIGNATURE_MALFORMED,
    SIGNATURE_MISSING,
    UNKNOWN_KEY,
    read_package,
    verify,
)


class PackageReaderTests(support.ComponentShellTestCase):
    """The package reader refuses with the host's codes."""

    def test_a_sound_package_reads(self) -> None:
        with support.TempDirectory() as root:
            package = support.build_package(root)

            self.assertEqual("acme.rating", package.manifest.component_id)
            self.assertEqual("1.0.0", package.manifest.component_version)
            self.assertEqual(["assemblies/Acme.RatingComponent.dll"], [f.path for f in package.manifest.files])
            self.assertEqual(64, len(package.manifest.digest()))
            self.assertEqual(package.files[package.manifest.entry_assembly], package.entry_assembly)

    def test_a_missing_manifest_is_refused(self) -> None:
        with support.TempDirectory() as root:
            self.assert_refused(MANIFEST_MISSING, read_package, str(root))

    def test_an_unsigned_package_is_refused(self) -> None:
        with support.TempDirectory() as root:
            support.build_package(root, write_signature=False)

            self.assert_refused(SIGNATURE_MISSING, read_package, str(root))

    def test_a_signature_that_is_not_base64_of_64_bytes_is_refused(self) -> None:
        with support.TempDirectory() as root:
            support.build_package(root)
            (root / "component.sig").write_text("AAA=", encoding="utf-8")

            self.assert_refused(SIGNATURE_MALFORMED, read_package, str(root))

    def test_an_assembly_edited_after_signing_is_refused(self) -> None:
        with support.TempDirectory() as root:
            support.build_package(root)
            (root / "assemblies" / "Acme.RatingComponent.dll").write_bytes(b"tampered bytes")

            self.assert_refused(HASH_MISMATCH, read_package, str(root))

    def test_a_listed_file_that_is_missing_is_refused(self) -> None:
        with support.TempDirectory() as root:
            support.build_package(root, assets={"assets/theme.json": b"{}"})
            (root / "assets" / "theme.json").unlink()

            self.assert_refused(FILE_MISSING, read_package, str(root))

    def test_a_path_that_leaves_the_package_is_refused(self) -> None:
        with support.TempDirectory() as root:
            support.build_package(root, assembly_path="../../escape.dll")

            # The path is checked while the manifest is parsed, before anything is opened.
            self.assert_refused(MANIFEST_INVALID, read_package, str(root))

    def test_an_unknown_manifest_member_is_refused(self) -> None:
        with support.TempDirectory() as root:
            support.build_package(root, extra_members={"extra": True})

            self.assert_refused(MANIFEST_INVALID, read_package, str(root))

    def test_a_manifest_that_is_not_json_is_refused(self) -> None:
        with support.TempDirectory() as root:
            support.build_package(root)
            (root / "component.json").write_text("{", encoding="utf-8")

            self.assert_refused(MANIFEST_INVALID, read_package, str(root))

    def test_a_publisher_id_with_a_dot_is_refused(self) -> None:
        with support.TempDirectory() as root:
            support.build_package(root, publisher_id="acme.inc")

            self.assert_refused(MANIFEST_INVALID, read_package, str(root))


class PackageVerificationTests(support.ComponentShellTestCase):
    """The signature over the canonical bytes, and the trust decisions around it."""

    def test_a_sound_package_verifies(self) -> None:
        with support.TempDirectory() as root:
            package = support.build_package(root)

            support.verify_package(package)

    def test_a_signature_over_other_bytes_is_refused(self) -> None:
        with support.TempDirectory() as root:
            package = support.build_package(root, signature=b"\x00" * 64)

            self.assert_refused(SIGNATURE_INVALID, verify, package, support.trust_for())

    def test_a_key_the_store_does_not_name_is_refused(self) -> None:
        with support.TempDirectory() as root:
            package = support.build_package(root, seed=support.OUTSIDER_SEED)

            self.assert_refused(UNKNOWN_KEY, verify, package, support.trust_for())

    def test_a_key_that_belongs_to_another_publisher_is_refused(self) -> None:
        with support.TempDirectory() as root:
            package = support.build_package(root)
            keys = support.trust_for(publisher="someone-else")

            self.assert_refused(PUBLISHER_MISMATCH, verify, package, keys)

    def test_a_trusted_key_with_no_publisher_is_refused(self) -> None:
        with support.TempDirectory() as root:
            package = support.build_package(root)
            keys = support.trust_for()
            for entry in keys.values():
                del entry["publisher_id"]

            self.assert_refused(PUBLISHER_MISMATCH, verify, package, keys)

    def test_a_public_key_filed_under_the_wrong_key_id_is_refused(self) -> None:
        with support.TempDirectory() as root:
            package = support.build_package(root)
            keys = support.trust_for()
            only = next(iter(keys.values()))
            other = support.trust_for(seed=support.OUTSIDER_SEED)
            wrong = {package.manifest.key_id: next(iter(other.values())) | {"publisher_id": only["publisher_id"]}}

            self.assert_refused(UNKNOWN_KEY, verify, package, wrong)

    def test_a_revoked_key_is_refused(self) -> None:
        with support.TempDirectory() as root:
            package = support.build_package(root)

            self.assert_refused(
                KEY_REVOKED, verify, package, support.trust_for(), revoked_keys=(package.manifest.key_id,)
            )

    def test_a_revoked_publisher_is_refused(self) -> None:
        with support.TempDirectory() as root:
            package = support.build_package(root)

            self.assert_refused(
                PUBLISHER_REVOKED, verify, package, support.trust_for(), revoked_publishers=("acme",)
            )

    def test_the_signature_is_over_the_canonical_bytes_not_the_file(self) -> None:
        with support.TempDirectory() as root:
            package = support.build_package(root)

            # Whitespace and key order on disk do not matter: the same fields sign the same bytes.
            body = package.manifest.canonical()
            (root / "component.json").write_text(json.dumps(body, indent=2), encoding="utf-8")
            reformatted = read_package(str(root))

            self.assertEqual(package.manifest.digest(), reformatted.manifest.digest())
            support.verify_package(reformatted)


class SetManifestTests(support.ComponentShellTestCase):
    """The content-addressed manifest, on the file the C# host's tests read as well."""

    def test_the_committed_sample_is_content_addressed(self) -> None:
        sample = self.manifest_sample()

        self.assertEqual(support.SET_SAMPLE.name, sample.file_name)
        self.assertEqual("acme-shell", sample.shell)
        self.assertEqual(1, len(sample.entries))
        self.assertEqual("acme.rating", sample.entries[0].component_id)

    def test_the_sample_is_written_exactly_as_the_tool_writes_it(self) -> None:
        sample = self.manifest_sample()

        rebuilt = set_manifest.build(sample.shell, sample.entries)

        self.assertEqual(support.SET_SAMPLE.read_text(encoding="utf-8"), rebuilt.text)
        self.assertEqual(sample.sha256, rebuilt.sha256)

    def test_a_manifest_round_trips(self) -> None:
        entry = set_manifest.Entry(
            component_id="acme.rating",
            component_version="1.0.0",
            publisher_id="acme",
            key_id="03396219237f75a64f12aeb7f39723ab",
            entry_assembly_sha256="1" * 64,
            manifest_sha256="2" * 64,
        )

        document = set_manifest.build("shell", [entry])
        parsed = set_manifest.parse(document.text)

        self.assertEqual(document.text, parsed.text)
        self.assertEqual(document.file_name, parsed.file_name)
        self.assertRegex(document.file_name, r"^component-set-[0-9a-f]{16}\.json$")

    def test_entries_are_sorted_by_id(self) -> None:
        first = set_manifest.Entry("b.two", "1.0.0", "acme", "0" * 32, "1" * 64, "2" * 64)
        second = set_manifest.Entry("a.one", "1.0.0", "acme", "0" * 32, "3" * 64, "4" * 64)

        document = set_manifest.build("shell", [first, second])

        self.assertEqual(["a.one", "b.two"], [entry.component_id for entry in document.entries])
        self.assertLess(document.text.index("a.one"), document.text.index("b.two"))

    def test_an_unsorted_document_is_refused(self) -> None:
        text = (
            '{"components":[{"component_id":"b.two","component_version":"1.0.0","entry_assembly_sha256":"'
            + "1" * 64
            + '","key_id":"'
            + "0" * 32
            + '","manifest_sha256":"'
            + "2" * 64
            + '","publisher_id":"acme"},{"component_id":"a.one","component_version":"1.0.0","entry_assembly_sha256":"'
            + "3" * 64
            + '","key_id":"'
            + "0" * 32
            + '","manifest_sha256":"'
            + "4" * 64
            + '","publisher_id":"acme"}],"schema":1,"shell":"s"}'
        )

        with self.assertRaises(set_manifest.ManifestError):
            set_manifest.parse(text)

    def test_an_unknown_member_is_refused(self) -> None:
        with self.assertRaises(set_manifest.ManifestError):
            set_manifest.parse('{"components":[],"schema":1,"shell":"s","extra":1}')

    def test_an_empty_set_is_sound(self) -> None:
        document = set_manifest.build("empty-shell", [])

        self.assertEqual([], list(set_manifest.parse(document.text).entries))

    def test_writing_names_the_file_after_its_content(self) -> None:
        with support.TempDirectory() as root:
            document = set_manifest.build("shell", [])
            path = support.Path(document.write(str(root)))

            self.assertEqual(document.file_name, path.name)
            self.assertEqual(document.sha256[:16], path.name[len("component-set-") : -len(".json")])

    def test_a_path_that_leaves_the_package_is_not_safe(self) -> None:
        from component_shell.package import is_safe_relative_path

        for path in ("../x.dll", "/etc/passwd", "C:/x.dll", "a\\b.dll", "a//b.dll", "./a.dll", ""):
            self.assertFalse(is_safe_relative_path(path), path)
        self.assertTrue(is_safe_relative_path("assemblies/Acme.RatingComponent.dll"))


if __name__ == "__main__":
    unittest.main()
