"""Licence metadata: packages carry Apache-2.0, the NOTICE holder, LICENSE and NOTICE."""

import shutil
import sys
import tarfile
import tempfile
import unittest
from pathlib import Path
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import build  # noqa: E402
import check  # noqa: E402

ROOT = Path(__file__).resolve().parents[3]
PACKAGING = ROOT / "packaging" / "linux"
BINARY_BYTES = b"#!/bin/sh\necho spot\n"


def notice_holder() -> str:
    """The one canonical holder: the ``Copyright`` line of NOTICE, without the year."""
    for line in (ROOT / "NOTICE").read_text(encoding="utf-8").splitlines():
        if line.startswith("Copyright "):
            return line.split(" ", 2)[2]
    raise AssertionError("NOTICE has no Copyright line")


class MetadataTest(unittest.TestCase):
    def test_spec_names_apache(self):
        spec = (PACKAGING / "rpm" / "spot.spec").read_text(encoding="utf-8")
        self.assertIn("License:        Apache-2.0", spec)
        self.assertIn("%license %{_licensedir}/spot/LICENSE", spec)
        self.assertIn("%license %{_licensedir}/spot/NOTICE", spec)
        self.assertRegex(spec, r"(?m)^Source\d+:\s+NOTICE$")

    def test_debian_copyright_is_dep5_apache_with_notice_holder(self):
        text = (PACKAGING / "debian" / "copyright").read_text(encoding="utf-8")
        self.assertTrue(text.startswith("Format: https://www.debian.org/doc/packaging-manuals/copyright-format/1.0/"))
        self.assertIn("License: Apache-2.0", text)
        self.assertIn("/usr/share/common-licenses/Apache-2.0", text)
        self.assertIn(f"Copyright: 2026 {notice_holder()}", text)

    def test_containerfile_label_and_files(self):
        text = (PACKAGING / "container" / "Containerfile").read_text(encoding="utf-8")
        self.assertIn('org.opencontainers.image.licenses="Apache-2.0"', text)
        self.assertRegex(text, r"(?m)^COPY LICENSE NOTICE ")

    def test_no_proprietary_wording_in_sources(self):
        for path in (PACKAGING / "rpm" / "spot.spec", PACKAGING / "debian" / "copyright",
                     PACKAGING / "container" / "Containerfile", ROOT / "tools" / "packaging" / "build.py"):
            lowered = path.read_text(encoding="utf-8").lower()
            self.assertNotIn("proprietary", lowered, path)
            self.assertNotIn("all rights reserved", lowered, path)


class BuildTest(unittest.TestCase):
    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory(prefix="spot-licence-test-")
        self.tmp = Path(self._tmp.name)
        self.binary = self.tmp / "spot"
        self.binary.write_bytes(BINARY_BYTES)
        self.binary.chmod(0o755)
        self.count = 0

    def tearDown(self):
        self._tmp.cleanup()

    def _build(self, kind="tarball"):
        self.count += 1
        return build.build(kind, self.binary, "1.2.3", self.tmp / f"out{self.count}", "x86_64")

    def _fake_root(self, *names):
        root = self.tmp / "root"
        root.mkdir(exist_ok=True)
        for name in names:
            shutil.copyfile(ROOT / name, root / name)
        return root

    def test_missing_licence_or_notice_fails_naming_the_file(self):
        for present, missing in ((("NOTICE",), "LICENSE"), (("LICENSE",), "NOTICE")):
            with self.subTest(missing=missing):
                root = self._fake_root(*present)
                for name in {"LICENSE", "NOTICE"} - set(present):
                    (root / name).unlink(missing_ok=True)
                with mock.patch.object(build, "REPO_ROOT", root):
                    with self.assertRaises(SystemExit) as ctx:
                        self._build()
                self.assertIn(missing, str(ctx.exception))
                self.assertNotEqual(ctx.exception.code, 0)

    def test_tarball_contains_both_files_unchanged(self):
        artifact = Path(self._build()["artifact"])
        with tarfile.open(artifact, "r:gz") as archive:
            self.assertEqual(archive.extractfile("LICENSE").read(), (ROOT / "LICENSE").read_bytes())
            self.assertEqual(archive.extractfile("NOTICE").read(), (ROOT / "NOTICE").read_bytes())
        self.assertTrue(check.check_tarball(artifact)["ok"])

    def test_tarball_check_reports_a_missing_notice(self):
        artifact = Path(self._build()["artifact"])
        stripped = self.tmp / "stripped.tar.gz"
        with tarfile.open(artifact, "r:gz") as src, tarfile.open(stripped, "w:gz") as dst:
            for member in src.getmembers():
                if member.name != "NOTICE":
                    dst.addfile(member, src.extractfile(member) if member.isfile() else None)
        result = check.check_tarball(stripped)
        self.assertFalse(result["ok"])
        self.assertTrue(any("NOTICE" in error for error in result["errors"]), result["errors"])

    def test_tarball_is_reproducible(self):
        first = Path(self._build()["artifact"]).read_bytes()
        second = Path(self._build()["artifact"]).read_bytes()
        self.assertEqual(first, second)

    @unittest.skipUnless(shutil.which("dpkg-deb"), "dpkg-deb not found")
    def test_deb_contains_both_files(self):
        import subprocess
        artifact = Path(self._build("deb")["artifact"])
        listing = subprocess.run(["dpkg-deb", "--contents", str(artifact)],
                                 capture_output=True, text=True, check=True).stdout
        self.assertIn("usr/share/doc/spot/LICENSE", listing)
        self.assertIn("usr/share/doc/spot/NOTICE", listing)
        self.assertTrue(check.check_deb(artifact, "1.2.3")["ok"])

    def test_deb_stage_list_names_both_files(self):
        # Runs without dpkg-deb: the staging list is part of the build source.
        source = Path(build.__file__).read_text(encoding="utf-8")
        self.assertIn('"usr/share/doc/spot/LICENSE"', source)
        self.assertIn('"usr/share/doc/spot/NOTICE"', source)


if __name__ == "__main__":
    unittest.main()
