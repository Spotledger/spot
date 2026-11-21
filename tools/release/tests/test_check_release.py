import contextlib
import io
import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))
import check_release  # noqa: E402

REAL_ROOT = Path(__file__).resolve().parents[3]
HOLDER = "Example Holder Limited"


def run(*argv):
    out, err = io.StringIO(), io.StringIO()
    with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
        try:
            rc = check_release.main(list(argv))
        except SystemExit as exc:  # argparse usage error
            rc = exc.code
    return rc, out.getvalue()


def put(root, rel, text):
    p = Path(root) / rel
    p.parent.mkdir(parents=True, exist_ok=True)
    p.write_text(text, encoding="utf-8")
    return p


def good_tree(root):
    put(root, "NOTICE", f"Product\nCopyright 2026 {HOLDER}\n")
    put(root, "Cargo.toml", '[workspace.package]\nversion = "0.1.0"\nlicense = "Apache-2.0"\n')
    put(root, "crates/a/Cargo.toml", '[package]\nname = "a"\nlicense.workspace = true\n')
    put(root, "packaging/linux/debian/copyright", f"Copyright: 2025 {HOLDER}\nLicense: Apache-2.0\n")
    put(root, "packaging/linux/rpm/spot.spec", "Name: spot\nLicense:        Apache-2.0\n")
    put(root, "packaging/linux/container/Containerfile",
        'LABEL org.opencontainers.image.licenses="Apache-2.0"\n')


class LicenceTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.root = Path(self.tmp.name)
        good_tree(self.root)

    def tearDown(self):
        self.tmp.cleanup()

    def bad(self, rel, text, needle):
        put(self.root, rel, text)
        rc, out = run("licence", "--root", str(self.root))
        self.assertEqual(rc, 1, out)
        self.assertIn(needle, out)
        self.assertIn(f"{rel}:", out)

    def test_clean_tree_passes(self):
        self.assertEqual(run("licence", "--root", str(self.root)), (0, ""))

    def test_proprietary_in_spec(self):
        self.bad("packaging/linux/rpm/spot.spec", "License: Proprietary\n", "proprietary")

    def test_licenseref_proprietary_case_insensitive(self):
        self.bad("packaging/linux/rpm/spot.spec", "# licenseref-proprietary\n", "proprietary")

    def test_all_rights_reserved(self):
        self.bad("packaging/linux/README.md", "Copyright 2026 x. ALL RIGHTS RESERVED\n", "All rights")

    def test_wrong_license_value(self):
        self.bad("packaging/linux/rpm/spot.spec", "License: MIT\n", "not Apache-2.0")

    def test_wrong_licenses_attribute(self):
        self.bad("packaging/linux/container/Containerfile",
                 'LABEL org.opencontainers.image.licenses="GPL-2.0"\n', "not Apache-2.0")

    def test_stale_holder_in_debian_copyright(self):
        self.bad("packaging/linux/debian/copyright",
                 "Copyright: 2026 Old Name Limited\nLicense: Apache-2.0\n", "stale copyright holder")

    def test_different_year_is_allowed(self):
        put(self.root, "packaging/linux/debian/copyright", f"Copyright: 2019-2024 {HOLDER}\n")
        self.assertEqual(run("licence", "--root", str(self.root))[0], 0)

    def test_cargo_license_wrong(self):
        self.bad("crates/a/Cargo.toml", '[package]\nname = "a"\nlicense = "MIT"\n', "not Apache-2.0")

    def test_missing_notice_fails_closed(self):
        (self.root / "NOTICE").unlink()
        rc, out = run("licence", "--root", str(self.root))
        self.assertEqual(rc, 1)
        self.assertIn("NOTICE:1:", out)

    def test_allowlist_ignores_tests_dir(self):
        put(self.root, "tools/release/tests/seed.spec", "License: Proprietary\n")
        self.assertEqual(run("licence", "--root", str(self.root))[0], 0)

    def test_every_finding_listed(self):
        put(self.root, "packaging/linux/rpm/spot.spec", "License: Proprietary\n")
        put(self.root, "packaging/linux/debian/copyright", "Copyright: 2026 Old Name\n")
        rc, out = run("licence", "--root", str(self.root))
        self.assertEqual(rc, 1)
        self.assertGreaterEqual(len(out.strip().splitlines()), 3)

    def test_real_tree_is_clean(self):
        rc, out = run("licence", "--root", str(REAL_ROOT))
        self.assertEqual(rc, 0, out)


class VersionTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.root = Path(self.tmp.name)
        good_tree(self.root)
        (self.root / "tools/packaging").mkdir(parents=True)
        shutil.copy(REAL_ROOT / "tools/packaging/build.py", self.root / "tools/packaging/build.py")

    def tearDown(self):
        self.tmp.cleanup()

    def ver(self, tag, *extra):
        return run("version", "--tag", tag, "--root", str(self.root), *extra)

    def test_matching_tag_passes_and_prints_spellings(self):
        rc, out = self.ver("v0.1.0")
        self.assertEqual(rc, 0, out)
        self.assertIn("rpm_release", out)
        self.assertIn("deb_version: 0.1.0", out)

    def test_tag_without_v(self):
        rc, out = self.ver("0.1.0")
        self.assertEqual(rc, 1)
        self.assertIn("leading v", out)

    def test_tag_with_trailing_newline_refused(self):
        rc, out = self.ver("v0.1.0\n")
        self.assertEqual(rc, 1)
        self.assertIn("leading v", out)

    def test_tag_differs_from_cargo(self):
        rc, out = self.ver("v0.2.0")
        self.assertEqual(rc, 1)
        self.assertIn("Cargo.toml", out)

    def test_rc_tag_matches(self):
        put(self.root, "Cargo.toml", '[workspace.package]\nversion = "0.1.0-rc.1"\n')
        rc, out = self.ver("v0.1.0-rc.1")
        self.assertEqual(rc, 0, out)
        self.assertIn("rpm_release: 0.rc.1", out)

    def test_bad_shapes(self):
        for tag in ("v1.2", "v1.2.3-", "v1.2.3-rc..1", "v1.2.3 ", "vx.y.z"):
            self.assertEqual(self.ver(tag)[0], 1, tag)

    def test_binary_version_checked(self):
        script = self.root / "fake.sh"
        script.write_text("#!/bin/sh\necho 'spot 0.1.0'\n")
        script.chmod(0o755)
        self.assertEqual(self.ver("v0.1.0", "--binary", str(script))[0], 0)
        script.write_text("#!/bin/sh\necho 'spot 9.9.9'\n")
        self.assertEqual(self.ver("v0.1.0", "--binary", str(script))[0], 1)

    def test_binary_without_version_output_skips(self):
        script = self.root / "quiet.sh"
        script.write_text("#!/bin/sh\nexit 0\n")
        script.chmod(0o755)
        rc, out = self.ver("v0.1.0", "--binary", str(script))
        self.assertEqual(rc, 0)
        self.assertIn("SKIP", out)

    def test_missing_binary_is_environment_error(self):
        self.assertEqual(self.ver("v0.1.0", "--binary", str(self.root / "nope"))[0], 2)

    def test_missing_cargo_is_environment_error(self):
        (self.root / "Cargo.toml").unlink()
        self.assertEqual(self.ver("v0.1.0")[0], 2)


class ChecksumTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.dir = Path(self.tmp.name)
        (self.dir / "b.deb").write_bytes(b"deb")
        (self.dir / "a.tar.gz").write_bytes(b"tar")

    def tearDown(self):
        self.tmp.cleanup()

    def test_format_sorted_lf_two_spaces(self):
        rc, _ = run("checksums", "--dir", str(self.dir))
        self.assertEqual(rc, 0)
        raw = (self.dir / "SHA256SUMS").read_bytes()
        self.assertNotIn(b"\r", raw)
        self.assertTrue(raw.endswith(b"\n"))
        lines = raw.decode().splitlines()
        self.assertEqual([ln.split("  ", 1)[1] for ln in lines], ["a.tar.gz", "b.deb"])
        for ln in lines:
            digest, name = ln.split("  ", 1)
            self.assertRegex(digest, r"^[0-9a-f]{64}$")
            self.assertNotIn("/", name)

    def test_sha256sum_verifies(self):
        if shutil.which("sha256sum") is None:
            self.skipTest("sha256sum not installed")
        run("checksums", "--dir", str(self.dir))
        proc = subprocess.run(["sha256sum", "-c", "SHA256SUMS"], cwd=self.dir, capture_output=True, text=True)
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)

    def test_deterministic_and_excludes_itself(self):
        run("checksums", "--dir", str(self.dir))
        first = (self.dir / "SHA256SUMS").read_bytes()
        run("checksums", "--dir", str(self.dir))
        self.assertEqual(first, (self.dir / "SHA256SUMS").read_bytes())
        self.assertNotIn(b"SHA256SUMS", first)

    def test_out_elsewhere(self):
        out = self.dir.parent / (self.dir.name + "-sums.txt")
        try:
            self.assertEqual(run("checksums", "--dir", str(self.dir), "--out", str(out))[0], 0)
            self.assertTrue(out.is_file())
            self.assertFalse((self.dir / "SHA256SUMS").exists())
        finally:
            out.unlink(missing_ok=True)

    def test_empty_dir_fails(self):
        with tempfile.TemporaryDirectory() as empty:
            self.assertEqual(run("checksums", "--dir", empty)[0], 1)

    def test_subdirectory_fails(self):
        (self.dir / "sub").mkdir()
        rc, out = run("checksums", "--dir", str(self.dir))
        self.assertEqual(rc, 1)
        self.assertIn("sub", out)
        self.assertFalse((self.dir / "SHA256SUMS").exists())

    def test_missing_dir_is_environment_error(self):
        self.assertEqual(run("checksums", "--dir", str(self.dir / "nope"))[0], 2)


class UsageTests(unittest.TestCase):
    def test_no_subcommand_is_usage_error(self):
        self.assertEqual(run()[0], 2)


if __name__ == "__main__":
    unittest.main()
