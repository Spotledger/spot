"""WP-OSS-08.T8: the release notes the workflow writes and verify_download.py --commit agree.

The shell lines of the build job's "write RELEASE-NOTES.md" step are extracted from
opensource/workflows/release.yml and executed here with SHA and TAG in the environment, a stub
dist/ is completed, check_release.py checksums runs on it, and verify_download.py must accept
the folder with the same commit. Nothing is mocked except the package contents.
"""
import io
import os
import shutil
import subprocess
import sys
import tarfile
import tempfile
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
REPO = HERE.parents[2]
WORKFLOW = REPO / "opensource" / "workflows" / "release.yml"
CHECK = REPO / "tools" / "release" / "check_release.py"
VERIFY = REPO / "tools" / "release" / "verify_download.py"
TAG = "v0.1.0-rc.1"
SHA = "0123456789abcdef0123456789abcdef01234567"
OTHER = "f" * 40
NAMES = {
    "tar": "spot-0.1.0-rc.1-linux-x86_64.tar.gz",
    "deb": "spot_0.1.0~rc.1_amd64.deb",
    "rpm": "spot-0.1.0-0.rc.1.x86_64.rpm",
    "sbom": "spot-0.1.0-rc.1.cdx.json",
}

try:
    import yaml
except ImportError:  # pragma: no cover
    yaml = None


def notes_step_script():
    doc = yaml.safe_load(WORKFLOW.read_text(encoding="utf-8"))
    for step in doc["jobs"]["build"]["steps"]:
        if isinstance(step, dict) and step.get("name") == "write RELEASE-NOTES.md":
            return step["run"]
    raise AssertionError("the build job has no step 'write RELEASE-NOTES.md'")


def stub_tarball(path):
    with tarfile.open(path, "w:gz") as tf:
        for name in ("pkg/LICENSE", "pkg/NOTICE", "pkg/bin/spot"):
            ti = tarfile.TarInfo(name)
            ti.size = 1
            tf.addfile(ti, io.BytesIO(b"x"))


def py(*args):
    return subprocess.run([sys.executable, *map(str, args)], capture_output=True, text=True, check=False)


@unittest.skipIf(yaml is None, "PyYAML is not installed")
class ReleasePairTests(unittest.TestCase):
    def setUp(self):
        self.tmp = Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, self.tmp, True)
        self.work = self.tmp / "work"
        self.dist = self.work / "dist"
        self.work.mkdir()

    def build_dist(self, sha=SHA):
        """Run the workflow's own notes step, add stub packages, then compute the checksums."""
        env = {"PATH": os.environ["PATH"], "SHA": sha, "TAG": TAG, "GITHUB_REPOSITORY": "Spotledger/spot"}
        done = subprocess.run(["bash", "-e", "-c", notes_step_script()], cwd=self.work, env=env,
                              capture_output=True, text=True, check=False)
        self.assertEqual(done.returncode, 0, done.stderr)
        stub_tarball(self.dist / NAMES["tar"])
        (self.dist / NAMES["deb"]).write_bytes(b"deb")
        (self.dist / NAMES["rpm"]).write_bytes(b"rpm")
        (self.dist / NAMES["sbom"]).write_text('{"bomFormat": "CycloneDX", "components": [{"name": "a"}]}')
        done = py(CHECK, "checksums", "--dir", self.dist)
        self.assertEqual(done.returncode, 0, done.stdout + done.stderr)

    def verify(self, commit=SHA):
        return py(VERIFY, self.dist, "--tag", TAG, "--commit", commit)

    def test_notes_step_output_shape(self):
        self.build_dist()
        text = (self.dist / "RELEASE-NOTES.md").read_text(encoding="utf-8")
        lines = [l for l in text.splitlines() if l.startswith("SOURCE-COMMIT")]
        self.assertEqual(lines, [f"SOURCE-COMMIT: {SHA}"])
        self.assertIn(TAG, text)
        self.assertIn(f"blob/{SHA}/docs/releasing.md", text)

    def test_notes_are_listed_in_the_checksums(self):
        self.build_dist()
        sums = (self.dist / "SHA256SUMS").read_text(encoding="utf-8")
        self.assertIn("  RELEASE-NOTES.md\n", sums)

    def test_pair_passes_with_the_gate_commit(self):
        self.build_dist()
        done = self.verify()
        self.assertEqual(done.returncode, 0, done.stdout + done.stderr)

    def test_a_different_commit_fails(self):
        self.build_dist()
        done = self.verify(OTHER)
        self.assertEqual(done.returncode, 1, done.stdout + done.stderr)
        self.assertIn("SOURCE-COMMIT", done.stdout + done.stderr)

    def test_a_second_source_commit_line_fails(self):
        self.build_dist()
        notes = self.dist / "RELEASE-NOTES.md"
        notes.write_text(notes.read_text(encoding="utf-8") + f"\nSOURCE-COMMIT: {SHA}\n", encoding="utf-8")
        # Re-checksum so the only finding left is the duplicated line.
        done = py(CHECK, "checksums", "--dir", self.dist)
        self.assertEqual(done.returncode, 0, done.stdout + done.stderr)
        done = self.verify()
        self.assertEqual(done.returncode, 1, done.stdout + done.stderr)
        self.assertIn("exactly one SOURCE-COMMIT", done.stdout + done.stderr)

    def test_notes_added_after_the_checksums_fail_as_an_extra_file(self):
        # What the old workflow did: the notes were not in dist/ when SHA256SUMS was computed.
        env = {"PATH": os.environ["PATH"], "SHA": SHA, "TAG": TAG, "GITHUB_REPOSITORY": "Spotledger/spot"}
        done = subprocess.run(["bash", "-e", "-c", notes_step_script()], cwd=self.work, env=env,
                              capture_output=True, text=True, check=False)
        self.assertEqual(done.returncode, 0, done.stderr)
        notes = self.dist / "RELEASE-NOTES.md"
        saved = notes.read_bytes()
        notes.unlink()
        stub_tarball(self.dist / NAMES["tar"])
        (self.dist / NAMES["deb"]).write_bytes(b"deb")
        (self.dist / NAMES["rpm"]).write_bytes(b"rpm")
        (self.dist / NAMES["sbom"]).write_text('{"bomFormat": "CycloneDX", "components": [{"name": "a"}]}')
        done = py(CHECK, "checksums", "--dir", self.dist)
        self.assertEqual(done.returncode, 0, done.stdout + done.stderr)
        notes.write_bytes(saved)
        done = self.verify()
        self.assertEqual(done.returncode, 1, done.stdout + done.stderr)
        self.assertIn("RELEASE-NOTES.md", done.stdout + done.stderr)

    def test_a_tampered_notes_file_fails_the_checksum(self):
        self.build_dist()
        notes = self.dist / "RELEASE-NOTES.md"
        notes.write_text(notes.read_text(encoding="utf-8") + "extra\n", encoding="utf-8")
        done = self.verify()
        self.assertEqual(done.returncode, 1, done.stdout + done.stderr)


if __name__ == "__main__":
    unittest.main()
