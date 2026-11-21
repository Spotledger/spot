"""Artefact discovery and version ordering.

The driver is handed a directory, not a manifest: it has to find the ``spot`` packages by
file name, order them the way the package managers do, and ignore anything else that lives
beside them (source RPMs, ``.rpmnew``/``.dpkg-dist`` backups, other projects' packages).
"""

import tempfile
import unittest
from pathlib import Path

import support  # noqa: F401 - puts the driver modules on sys.path

import packages


class VersionKeyTests(unittest.TestCase):
    def test_numeric_order_not_lexicographic(self):
        self.assertLess(packages.version_key("1.2.9"), packages.version_key("1.2.10"))
        self.assertLess(packages.version_key("1.9.0"), packages.version_key("1.10.0"))

    def test_release_sorts_after_a_pre_release(self):
        self.assertLess(
            packages.version_key("1.2.3-rc.1"), packages.version_key("1.2.3")
        )
        self.assertLess(
            packages.version_key("1.2.3~rc.1"), packages.version_key("1.2.3")
        )

    def test_equal_versions_compare_equal(self):
        self.assertEqual(packages.version_key("1.2.3"), packages.version_key("1.2.3"))


class ParseTests(unittest.TestCase):
    def test_rpm_name(self):
        artefact = packages.parse(Path("spot-1.2.3-1.fc44.x86_64.rpm"))
        self.assertIsNotNone(artefact)
        self.assertEqual(artefact.package_format, "rpm")
        self.assertEqual(artefact.name, "spot")
        self.assertEqual(artefact.version, "1.2.3")
        self.assertEqual(artefact.release, "1.fc44")
        self.assertEqual(artefact.arch, "x86_64")
        self.assertEqual(artefact.display_version, "1.2.3-1.fc44")

    def test_rpm_pre_release(self):
        artefact = packages.parse(Path("spot-2.0.0-0.rc.1.fc44.x86_64.rpm"))
        self.assertIsNotNone(artefact)
        self.assertEqual(artefact.version, "2.0.0")
        self.assertEqual(artefact.release, "0.rc.1.fc44")

    def test_deb_name(self):
        artefact = packages.parse(Path("spot_1.2.3_amd64.deb"))
        self.assertIsNotNone(artefact)
        self.assertEqual(artefact.package_format, "deb")
        self.assertEqual(artefact.version, "1.2.3")
        self.assertEqual(artefact.arch, "amd64")
        self.assertEqual(artefact.display_version, "1.2.3")

    def test_deb_pre_release(self):
        artefact = packages.parse(Path("spot_2.0.0~rc.1_amd64.deb"))
        self.assertIsNotNone(artefact)
        self.assertEqual(artefact.version, "2.0.0~rc.1")

    def test_ignores_everything_that_is_not_ours(self):
        for name in (
            "spot-1.2.3-1.fc44.src.rpm",
            "spot.toml.rpmnew",
            "spot.toml.dpkg-dist",
            "other-1.2.3-1.fc44.x86_64.rpm",
            "other_1.2.3_amd64.deb",
            "README.md",
            "spot-1.2.3-1.fc44.x86_64.rpm.asc",
        ):
            with self.subTest(name=name):
                self.assertIsNone(packages.parse(Path(name)))


class DiscoverTests(unittest.TestCase):
    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory(prefix="spot-verify-packages-")
        self.directory = Path(self._tmp.name)

    def tearDown(self):
        self._tmp.cleanup()

    def test_missing_directory_is_empty_not_an_error(self):
        self.assertEqual(packages.discover(self.directory / "absent"), ())
        self.assertEqual(packages.discover(None), ())

    def test_orders_oldest_first_across_formats(self):
        support.write_packages(
            self.directory,
            ["spot_1.2.4_amd64.deb", "spot-1.2.3-1.fc44.x86_64.rpm", "spot_1.2.3_amd64.deb"],
        )
        artefacts = packages.discover(self.directory)
        self.assertEqual([artefact.version for artefact in artefacts],
                         ["1.2.3", "1.2.3", "1.2.4"])

    def test_for_format_and_latest(self):
        support.write_packages(
            self.directory, ["spot-1.2.3-1.fc44.x86_64.rpm", "spot-1.2.4-1.fc44.x86_64.rpm",
                             "spot_1.2.3_amd64.deb"],
        )
        artefacts = packages.discover(self.directory)
        rpms = packages.for_format(artefacts, "rpm")
        self.assertEqual(len(rpms), 2)
        self.assertEqual(packages.latest(artefacts, "rpm").version, "1.2.4")
        self.assertEqual(packages.latest(artefacts, "deb").version, "1.2.3")

    def test_upgrade_pair_needs_two_versions(self):
        support.write_packages(self.directory, ["spot-1.2.3-1.fc44.x86_64.rpm"])
        artefacts = packages.discover(self.directory)
        self.assertIsNone(packages.upgrade_pair(artefacts, "rpm"))

    def test_upgrade_pair_returns_oldest_and_newest(self):
        support.write_packages(
            self.directory,
            ["spot-1.2.4-1.fc44.x86_64.rpm", "spot-1.2.3-1.fc44.x86_64.rpm",
             "spot-1.2.5-1.fc44.x86_64.rpm"],
        )
        pair = packages.upgrade_pair(packages.discover(self.directory), "rpm")
        self.assertIsNotNone(pair)
        self.assertEqual(pair[0].version, "1.2.3")
        self.assertEqual(pair[1].version, "1.2.5")

    def test_identical_version_pair_is_not_an_upgrade(self):
        support.write_packages(
            self.directory, ["spot-1.2.3-1.fc44.x86_64.rpm", "spot-1.2.3-1.fc44.x86_64.rpm"]
        )
        # Two files with the same name cannot coexist on disk; the same version with a
        # different release is the realistic case and it is still an upgrade.
        artefacts = packages.discover(self.directory)
        self.assertEqual(len(artefacts), 1)
        self.assertIsNone(packages.upgrade_pair(artefacts, "rpm"))


if __name__ == "__main__":
    unittest.main()
