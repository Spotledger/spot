import contextlib
import hashlib
import io
import os
import sys
import tarfile
import tempfile
import unittest
from pathlib import Path
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import build  # noqa: E402

BINARY_BYTES = b"#!/bin/sh\necho spot\n"


class BuildTest(unittest.TestCase):
    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory(prefix="spot-build-test-")
        self.tmp = Path(self._tmp.name)
        self.binary = self.tmp / "spot"
        self.binary.write_bytes(BINARY_BYTES)
        self.binary.chmod(0o755)

    def tearDown(self):
        self._tmp.cleanup()

    def _build(self, kind="tarball", version="1.2.3", arch="x86_64", binary=None):
        out = self.tmp / f"out-{kind}-{version}-{len(list(self.tmp.iterdir()))}"
        return build.build(kind, binary or self.binary, version, out, arch)

    def test_tarball_is_byte_identical_across_runs(self):
        first = self._build()
        second = self._build()
        one = Path(first["artifact"]).read_bytes()
        two = Path(second["artifact"]).read_bytes()
        self.assertEqual(hashlib.sha256(one).hexdigest(), hashlib.sha256(two).hexdigest())
        self.assertEqual(one, two)

    def test_tarball_name_and_layout(self):
        result = self._build()
        artifact = Path(result["artifact"])
        self.assertEqual(artifact.name, "spot-1.2.3-linux-x86_64.tar.gz")
        with tarfile.open(artifact, "r:gz") as archive:
            names = archive.getnames()
            self.assertEqual(names, sorted(names))
            self.assertEqual(
                set(names),
                {
                    "bin", "bin/spot", "etc", "etc/spot.toml", "etc/environment",
                    "systemd", "systemd/spot.service", "systemd/spot.sysusers",
                    "systemd/spot.tmpfiles", "LICENSE", "README.install",
                },
            )
            spot = archive.getmember("bin/spot")
            self.assertEqual(spot.mode & 0o777, 0o755)
            self.assertEqual(spot.uid, 0)
            self.assertEqual(spot.gid, 0)
            self.assertEqual(spot.uname, "")
            self.assertEqual(spot.gname, "")
            config = archive.getmember("etc/spot.toml")
            self.assertEqual(config.mode & 0o777, 0o640)
            readme = archive.extractfile("README.install").read().decode()
            self.assertIn("1.2.3", readme)

    def test_tarball_honours_source_date_epoch(self):
        epoch = 1700000000
        with mock.patch.dict(os.environ, {"SOURCE_DATE_EPOCH": str(epoch)}):
            result = self._build()
        with tarfile.open(result["artifact"], "r:gz") as archive:
            self.assertEqual(archive.getmember("bin/spot").mtime, epoch)

    def test_default_epoch_makes_runs_identical_without_environment(self):
        with mock.patch.dict(os.environ, {}, clear=False):
            os.environ.pop("SOURCE_DATE_EPOCH", None)
            first = self._build()
            second = self._build()
        self.assertEqual(Path(first["artifact"]).read_bytes(), Path(second["artifact"]).read_bytes())

    def test_paths_with_spaces(self):
        spaced_dir = self.tmp / "bin dir"
        spaced_dir.mkdir()
        binary = spaced_dir / "spot binary"
        binary.write_bytes(BINARY_BYTES)
        binary.chmod(0o755)
        out = self.tmp / "output dir"
        result = build.build("tarball", binary, "1.2.3", out, "x86_64")
        self.assertTrue(Path(result["artifact"]).is_file())
        self.assertEqual(Path(result["artifact"]).parent, out.resolve())

    def test_missing_binary_is_an_error(self):
        with self.assertRaises(SystemExit):
            build.build("tarball", self.tmp / "absent", "1.2.3", self.tmp / "out", "x86_64")

    def test_skips_cleanly_when_tools_are_missing(self):
        for kind, tool in (("rpm", "rpmbuild"), ("deb", "dpkg-deb"), ("container", "podman")):
            with self.subTest(kind=kind):
                stdout = io.StringIO()
                with mock.patch.object(build.shutil, "which", return_value=None), \
                        contextlib.redirect_stdout(stdout):
                    result = build.build(kind, self.binary, "1.2.3", self.tmp / f"out-{kind}", "x86_64")
                self.assertEqual(result["status"], "skipped")
                self.assertIn(tool, result["reason"])
                self.assertIn(f"SKIP {kind}", stdout.getvalue())

    def test_version_mapping(self):
        self.assertEqual(
            build.package_versions("1.2.3"),
            {"version": "1.2.3", "rpm_version": "1.2.3", "rpm_release": "1",
             "deb_version": "1.2.3"},
        )
        self.assertEqual(
            build.package_versions("2.0.0-rc.1"),
            {"version": "2.0.0-rc.1", "rpm_version": "2.0.0", "rpm_release": "0.rc.1",
             "deb_version": "2.0.0~rc.1"},
        )
        self.assertEqual(
            build.package_versions("v3.4.5"),
            {"version": "v3.4.5", "rpm_version": "3.4.5", "rpm_release": "1",
             "deb_version": "3.4.5"},
        )

    def test_invalid_versions_are_rejected(self):
        for version in ("1.2.3.4", "1.2.x", "1.2.3-", "1.2.3-rc_main"):
            with self.subTest(version=version), self.assertRaises(SystemExit):
                build.package_versions(version)

    def test_rendered_spec_maps_prerelease(self):
        text, rpm_version, release = build.render_spec("2.0.0-rc.1", build.DEFAULT_EPOCH)
        self.assertEqual((rpm_version, release), ("2.0.0", "0.rc.1"))
        self.assertIn("Version:        2.0.0", text)
        self.assertIn("Release:        0.rc.1", text)
        self.assertIn("build of 2.0.0-0.rc.1 from the prebuilt binary", text)


if __name__ == "__main__":
    unittest.main()
