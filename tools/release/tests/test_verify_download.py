import contextlib
import hashlib
import io
import json
import os
import shutil
import sys
import tarfile
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))
import verify_download as vd  # noqa: E402

TAG = "v0.1.0-rc.1"
COMMIT = "a" * 40
NAMES = {
    "tar": "spot-0.1.0-rc.1-linux-x86_64.tar.gz",
    "deb": "spot_0.1.0~rc.1_amd64.deb",
    "rpm": "spot-0.1.0-0.rc.1.x86_64.rpm",
    "sbom": "spot-0.1.0-rc.1.cdx.json",
}


def run(argv):
    out, err = io.StringIO(), io.StringIO()
    with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
        try:
            code = vd.main(argv)
        except SystemExit as exc:  # argparse usage error
            code = exc.code
    return code, out.getvalue() + err.getvalue()


def make_tarball(path, members=("pkg/LICENSE", "pkg/NOTICE", "pkg/bin/spot")):
    with tarfile.open(path, "w:gz") as tf:
        for m in members:
            data = b"x"
            ti = tarfile.TarInfo(m)
            ti.size = len(data)
            tf.addfile(ti, io.BytesIO(data))


def write_sums(d, extra=()):
    lines = []
    for p in sorted(Path(d).iterdir()):
        if p.name == "SHA256SUMS":
            continue
        lines.append(f"{hashlib.sha256(p.read_bytes()).hexdigest()}  {p.name}")
    lines.extend(extra)
    (Path(d) / "SHA256SUMS").write_text("\n".join(lines) + "\n")


class Base(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.mkdtemp()
        self.addCleanup(shutil.rmtree, self.tmp, True)
        self.d = Path(self.tmp)
        make_tarball(self.d / NAMES["tar"])
        (self.d / NAMES["deb"]).write_bytes(b"deb")
        (self.d / NAMES["rpm"]).write_bytes(b"rpm")
        self.sbom({"bomFormat": "CycloneDX", "components": [{"name": "a"}]})
        (self.d / "RELEASE-NOTES.md").write_text(f"Status: pre-release\nSOURCE-COMMIT: {COMMIT}\n")
        write_sums(self.d)

    def sbom(self, doc):
        (self.d / NAMES["sbom"]).write_text(json.dumps(doc))

    def verify(self, *extra):
        return run([str(self.d), "--tag", TAG, *extra])


class Good(Base):
    def test_good_folder(self):
        code, out = self.verify("--commit", COMMIT)
        self.assertEqual(code, 0, out)

    def test_binary_marker_accepted(self):
        text = (self.d / "SHA256SUMS").read_text().replace("  ", " *")
        (self.d / "SHA256SUMS").write_text(text)
        self.assertEqual(self.verify()[0], 0)

    def test_final_version(self):
        for p in list(self.d.iterdir()):
            p.unlink()
        make_tarball(self.d / "spot-1.2.3-linux-aarch64.tar.gz")
        (self.d / "spot_1.2.3_arm64.deb").write_bytes(b"d")
        (self.d / "spot-1.2.3-1.aarch64.rpm").write_bytes(b"r")
        (self.d / "spot-1.2.3.cdx.json").write_text(json.dumps({"bomFormat": "CycloneDX", "components": [1]}))
        write_sums(self.d)
        self.assertEqual(run([str(self.d), "--tag", "v1.2.3"])[0], 0)


class Bad(Base):
    def assertFails(self, needle, *extra):
        code, out = self.verify(*extra)
        self.assertEqual(code, 1, out)
        self.assertIn(needle, out)

    def test_flipped_byte(self):
        p = self.d / NAMES["deb"]
        p.write_bytes(b"dec")
        self.assertFails("checksum mismatch")

    def test_missing_file(self):
        (self.d / NAMES["rpm"]).unlink()
        code, out = self.verify()
        self.assertEqual(code, 1)
        self.assertIn("listed in SHA256SUMS but missing", out)
        self.assertIn("no .rpm file", out)

    def test_extra_file(self):
        (self.d / "extra.txt").write_text("hi")
        self.assertFails("extra.txt: present but not listed")

    def test_sbom_not_cyclonedx(self):
        self.sbom({"bomFormat": "SPDX", "components": [1]})
        write_sums(self.d)
        self.assertFails("not 'CycloneDX'")

    def test_sbom_no_components(self):
        self.sbom({"bomFormat": "CycloneDX", "components": []})
        write_sums(self.d)
        self.assertFails("no components")

    def test_sbom_not_json(self):
        (self.d / NAMES["sbom"]).write_text("nope")
        write_sums(self.d)
        self.assertFails("not valid JSON")

    def test_missing_sums(self):
        (self.d / "SHA256SUMS").unlink()
        self.assertFails("SHA256SUMS: missing")

    def test_empty_sums(self):
        (self.d / "SHA256SUMS").write_text("")
        self.assertFails("SHA256SUMS: empty")

    def test_malformed_line(self):
        with (self.d / "SHA256SUMS").open("a") as fh:
            fh.write("garbage\n")
        self.assertFails("malformed line")

    def test_path_traversal_names(self):
        digest = "0" * 64
        for bad in ("../evil", "/etc/passwd", "sub/file", "a\\b", "..", "x..y"):
            write_sums(self.d, extra=[f"{digest}  {bad}"])
            code, out = self.verify()
            self.assertEqual(code, 1, bad)
            self.assertIn("refused name", out, bad)

    def test_duplicate_entry(self):
        lines = (self.d / "SHA256SUMS").read_text().splitlines()
        (self.d / "SHA256SUMS").write_text("\n".join(lines + [lines[0]]) + "\n")
        self.assertFails("listed twice")

    def test_symlink_refused(self):
        os.symlink("/etc/hostname", self.d / "link")
        self.assertFails("link: not a plain file")

    def test_subdirectory_refused(self):
        (self.d / "sub").mkdir()
        self.assertFails("sub: not a plain file")

    def test_wrong_version_name(self):
        (self.d / NAMES["deb"]).rename(self.d / "spot_0.1.0_amd64.deb")
        write_sums(self.d)
        self.assertFails("does not match version 0.1.0-rc.1")

    def test_two_debs(self):
        (self.d / "spot_0.1.0~rc.1_arm64.deb").write_bytes(b"x")
        write_sums(self.d)
        self.assertFails("expected one .deb file")

    def test_tarball_without_notice(self):
        make_tarball(self.d / NAMES["tar"], members=("pkg/LICENSE",))
        write_sums(self.d)
        self.assertFails("NOTICE missing")

    def test_tarball_unsafe_member(self):
        make_tarball(self.d / NAMES["tar"], members=("../LICENSE", "NOTICE", "LICENSE"))
        write_sums(self.d)
        self.assertFails("unsafe member name")

    def test_tarball_corrupt(self):
        (self.d / NAMES["tar"]).write_bytes(b"not a tarball")
        write_sums(self.d)
        self.assertFails("not a readable gzip tarball")

    def test_commit_mismatch(self):
        self.assertFails("is not " + "b" * 40, "--commit", "b" * 40)

    def test_commit_missing_notes(self):
        (self.d / "RELEASE-NOTES.md").unlink()
        write_sums(self.d)
        self.assertFails("cannot check --commit", "--commit", COMMIT)

    def test_commit_without_line(self):
        (self.d / "RELEASE-NOTES.md").write_text("nothing here\n")
        write_sums(self.d)
        self.assertFails("expected exactly one SOURCE-COMMIT", "--commit", COMMIT)

    def test_require_signed_without_signature(self):
        self.assertFails("signature required", "--require-signed")

    def test_signature_file_is_not_an_extra_file(self):
        (self.d / "SHA256SUMS.asc").write_text("junk")
        code, out = self.verify()  # not required: tolerated
        self.assertEqual(code, 0, out)

    def test_all_findings_listed(self):
        (self.d / NAMES["deb"]).write_bytes(b"changed")
        (self.d / "extra").write_text("x")
        code, out = self.verify()
        self.assertEqual(code, 1)
        self.assertIn("checksum mismatch", out)
        self.assertIn("extra: present but not listed", out)


class Usage(Base):
    def test_bad_tag(self):
        self.assertEqual(run([str(self.d), "--tag", "0.1.0"])[0], 2)

    def test_bad_commit(self):
        self.assertEqual(run([str(self.d), "--tag", TAG, "--commit", "abc"])[0], 2)

    def test_not_a_directory(self):
        self.assertEqual(run([str(self.d / "nope"), "--tag", TAG])[0], 2)

    def test_missing_tag_argument(self):
        self.assertEqual(run([str(self.d)])[0], 2)


class Helpers(unittest.TestCase):
    def test_safe_name(self):
        self.assertIsNone(vd.safe_name("spot_0.1.0~rc.1_amd64.deb"))
        for bad in ("", ".", "a/b", "..", "-rf", "a\x00b", "C:evil"):
            self.assertIsNotNone(vd.safe_name(bad), bad)


if __name__ == "__main__":
    unittest.main()
