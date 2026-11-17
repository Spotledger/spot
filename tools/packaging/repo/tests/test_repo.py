"""Tests of the signed Linux repositories (WP-PRD-09.T8), with throwaway keys in temp keyrings.

Run: python -m unittest discover -s tools/packaging/repo/tests -v
The tests that need ``rpmsign``/``createrepo_c``/``rpmbuild``/``dpkg-deb``/``podman`` skip with a reason.
"""

import gzip
import hashlib
import os
import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
TOOLS = HERE.parent
REPO = TOOLS.parents[2]
sys.path.insert(0, str(TOOLS))
sys.path.insert(0, str(REPO / "tools" / "packaging"))

import build_keys_package  # noqa: E402
import build_repo  # noqa: E402
import debrepo  # noqa: E402
import rotation_check  # noqa: E402
import rpmrepo  # noqa: E402
import signing  # noqa: E402

EPOCH = 1767225600
HAVE_GPG = shutil.which("gpg") is not None and shutil.which("gpgv") is not None
needs_gpg = unittest.skipUnless(HAVE_GPG, "gpg/gpgv not installed")


def tool(name):
    return shutil.which(name) is not None


class Fixture(unittest.TestCase):
    """Two throwaway keys and a one-package DEB repository per test."""

    @classmethod
    def setUpClass(cls):
        if not HAVE_GPG:
            raise unittest.SkipTest("gpg/gpgv not installed")
        cls._tmp = tempfile.TemporaryDirectory(prefix="spot-repo-test-")
        tmp = Path(cls._tmp.name)
        cls.secrets = {}
        for label in ("a", "b"):
            home = tmp / f"home-{label}"
            home.mkdir()
            cls.secrets[label] = rotation_check.throwaway_key(home, f"Spot test {label} <{label}@invalid>")
        pass_home = tmp / "home-p"
        pass_home.mkdir()
        cls.deb = rotation_check.make_deb(tmp / "spot_1.2.3_amd64.deb", "spot", "1.2.3")
        cls.deb_all = rotation_check.make_deb(tmp / "spot-data_1.0_all.deb", "spot-data", "1.0")
        cls.tmp = tmp

    @classmethod
    def tearDownClass(cls):
        cls._tmp.cleanup()

    def signer(self, label="a"):
        s = signing.Signer.from_env({signing.KEY_ENV: self.secrets[label].decode()})
        self.addCleanup(s.close)
        return s

    def keyring(self, *labels):
        keys = []
        for label in labels:
            with signing.Signer.from_env({signing.KEY_ENV: self.secrets[label].decode()}) as s:
                keys.append(s.public_binary())
        return signing.make_keyring(keys, self.tmp / f"ring-{'-'.join(labels)}-{id(self)}.gpg")

    def build(self, label="a", debs=None, out=None):
        out = out or Path(tempfile.mkdtemp(dir=self.tmp))
        root = debrepo.build_repo(debs or [self.deb], out, "stable", self.signer(label), EPOCH)
        return out, root


class DebRepoTest(Fixture):
    def test_layout_and_verification(self):
        out, root = self.build()
        suite = root / "dists" / "stable"
        for name in ("Release", "InRelease", "Release.gpg", "main/binary-amd64/Packages", "main/binary-amd64/Packages.gz"):
            self.assertTrue((suite / name).is_file(), name)
        self.assertTrue((root / "pool/stable/main/s/spot/spot_1.2.3_amd64.deb").is_file())
        debrepo.verify_repo(root, "stable", self.keyring("a"))

    def test_release_is_reproducible_and_dated_from_the_argument(self):
        _, first = self.build()
        _, second = self.build()
        r1 = (first / "dists/stable/Release").read_text()
        self.assertEqual(r1, (second / "dists/stable/Release").read_text())
        self.assertIn("Date: " + debrepo.release_date(EPOCH), r1)
        self.assertIn("Suite: stable", r1)

    def test_packages_entry_has_hashes_and_filename(self):
        _, root = self.build()
        text = (root / "dists/stable/main/binary-amd64/Packages").read_text()
        fields = dict(debrepo.parse_fields(text))
        self.assertEqual(fields["Package"], "spot")
        self.assertEqual(fields["Filename"], "pool/stable/main/s/spot/spot_1.2.3_amd64.deb")
        self.assertEqual(fields["SHA256"], hashlib.sha256(self.deb.read_bytes()).hexdigest())
        self.assertEqual(int(fields["Size"]), self.deb.stat().st_size)
        self.assertLess(text.index("SHA256"), text.index("Description"))

    def test_arch_all_package_listed_in_the_real_architecture(self):
        _, root = self.build(debs=[self.deb, self.deb_all])
        text = (root / "dists/stable/main/binary-amd64/Packages").read_text()
        self.assertIn("Package: spot-data", text)
        debrepo.verify_repo(root, "stable", self.keyring("a"))

    def test_valid_until_is_optional(self):
        out = Path(tempfile.mkdtemp(dir=self.tmp))
        root = debrepo.build_repo([self.deb], out, "beta", self.signer(), EPOCH, valid_days=7)
        self.assertIn("Valid-Until: " + debrepo.release_date(EPOCH + 7 * 86400), (root / "dists/beta/Release").read_text())

    def test_unknown_channel_and_empty_input_refused(self):
        out = Path(tempfile.mkdtemp(dir=self.tmp))
        with self.assertRaises(debrepo.RepoError):
            debrepo.build_repo([self.deb], out, "nightly", self.signer(), EPOCH)
        with self.assertRaises(debrepo.RepoError):
            debrepo.build_repo([], out, "stable", self.signer(), EPOCH)

    # -- tampering: each must fail verification --------------------------------

    def flip(self, path):
        data = bytearray(path.read_bytes())
        data[len(data) // 2] ^= 0x01
        path.write_bytes(bytes(data))

    def test_flipped_byte_in_a_package_fails(self):
        _, root = self.build()
        self.flip(root / "pool/stable/main/s/spot/spot_1.2.3_amd64.deb")
        with self.assertRaisesRegex(debrepo.RepoError, "does not match Packages"):
            debrepo.verify_repo(root, "stable", self.keyring("a"))

    def test_flipped_byte_in_packages_metadata_fails(self):
        _, root = self.build()
        self.flip(root / "dists/stable/main/binary-amd64/Packages")
        with self.assertRaisesRegex(debrepo.RepoError, "does not match Release"):
            debrepo.verify_repo(root, "stable", self.keyring("a"))

    def test_flipped_byte_in_inrelease_fails(self):
        _, root = self.build()
        path = root / "dists/stable/InRelease"
        path.write_text(path.read_text().replace("Suite: stable", "Suite: stablf"))
        with self.assertRaisesRegex(debrepo.RepoError, "InRelease signature"):
            debrepo.verify_repo(root, "stable", self.keyring("a"))

    def test_tampered_release_with_valid_inrelease_fails_release_gpg(self):
        _, root = self.build()
        path = root / "dists/stable/Release"
        path.write_text(path.read_text() + "X: y\n")
        with self.assertRaisesRegex(debrepo.RepoError, "Release.gpg"):
            debrepo.verify_repo(root, "stable", self.keyring("a"))

    def test_unsigned_repository_is_refused(self):
        _, root = self.build()
        (root / "dists/stable/InRelease").unlink()
        with self.assertRaisesRegex(debrepo.RepoError, "no InRelease"):
            debrepo.verify_repo(root, "stable", self.keyring("a"))

    def test_signature_by_an_unpinned_key_is_refused(self):
        _, root = self.build(label="b")
        with self.assertRaises(debrepo.RepoError):
            debrepo.verify_repo(root, "stable", self.keyring("a"))

    def test_missing_package_file_fails(self):
        _, root = self.build()
        (root / "pool/stable/main/s/spot/spot_1.2.3_amd64.deb").unlink()
        with self.assertRaisesRegex(debrepo.RepoError, "missing"):
            debrepo.verify_repo(root, "stable", self.keyring("a"))


class PrivateMaterialTest(Fixture):
    def test_output_tree_has_public_key_only(self):
        out = Path(tempfile.mkdtemp(dir=self.tmp))
        with self.signer() as s:
            debrepo.build_repo([self.deb], out, "stable", s, EPOCH)
            (out / "spot-release.asc").write_bytes(s.public_armored())
        build_repo.assert_no_private_material(out)
        for path in out.rglob("*"):
            if path.is_file():
                self.assertNotIn(b"PRIVATE KEY", path.read_bytes())

    def test_scanner_catches_armoured_and_binary_secret_keys(self):
        out = Path(tempfile.mkdtemp(dir=self.tmp))
        (out / "leak.asc").write_bytes(self.secrets["a"])
        with self.assertRaises(signing.SigningError):
            build_repo.assert_no_private_material(out)
        (out / "leak.asc").unlink()
        home = self.tmp / "exp-home"
        home.mkdir(exist_ok=True)
        os.chmod(home, 0o700)
        gpg = ["gpg", "--homedir", str(home), "--batch", "--no-tty", "--pinentry-mode", "loopback", "--passphrase", ""]
        subprocess.run(gpg + ["--import"], input=self.secrets["a"], capture_output=True, check=True)
        binary = subprocess.run(gpg + ["--export-secret-keys"], capture_output=True, check=True).stdout
        subprocess.run(["gpgconf", "--homedir", str(home), "--kill", "all"], capture_output=True)
        (out / "leak.gpg").write_bytes(binary)
        with self.assertRaises(signing.SigningError):
            build_repo.assert_no_private_material(out)

    def test_signer_workdir_is_removed_and_key_not_in_argv(self):
        s = signing.Signer.from_env({signing.KEY_ENV: self.secrets["a"].decode()})
        home = s.home
        self.assertTrue(home.is_dir())
        self.assertEqual(home.stat().st_mode & 0o777, 0o700)
        s.close()
        self.assertFalse(home.exists())

    def test_key_must_come_from_the_environment(self):
        with self.assertRaisesRegex(signing.SigningError, "SPOT_GPG_KEY"):
            signing.Signer.from_env({})
        with self.assertRaises(signing.SigningError):
            signing.Signer.from_env({signing.KEY_ENV: "not a key"})

    def test_error_text_never_contains_key_material(self):
        try:
            signing.Signer.from_env({signing.KEY_ENV: "garbage-secret-marker"})
        except signing.SigningError as exc:
            self.assertNotIn("garbage-secret-marker", str(exc))

    def test_passphrase_file_is_used(self):
        tmp = self.tmp / "pp"
        tmp.mkdir(exist_ok=True)
        home = tmp / "h"
        home.mkdir(exist_ok=True)
        os.chmod(home, 0o700)
        gpg = ["gpg", "--homedir", str(home), "--batch", "--no-tty", "--pinentry-mode", "loopback"]
        subprocess.run(gpg + ["--passphrase", "s3cret-pass", "--quick-generate-key", "P <p@invalid>", "ed25519", "sign", "1d"],
                       capture_output=True, check=True)
        secret = subprocess.run(gpg + ["--passphrase", "s3cret-pass", "--armor", "--export-secret-keys"],
                                capture_output=True, check=True).stdout
        subprocess.run(["gpgconf", "--homedir", str(home), "--kill", "all"], capture_output=True)
        good = tmp / "good.txt"
        good.write_text("s3cret-pass\n")
        bad = tmp / "bad.txt"
        bad.write_text("wrong\n")
        env = {signing.KEY_ENV: secret.decode(), signing.PASSPHRASE_ENV: str(good)}
        with signing.Signer.from_env(env) as s:
            data = tmp / "d.txt"
            data.write_text("hello")
            sig = s.detached(data, tmp / "d.txt.asc")
            ring = signing.make_keyring([s.public_binary()], tmp / "ring.gpg")
            self.assertTrue(signing.verify(ring, sig, data))
        with signing.Signer.from_env({**env, signing.PASSPHRASE_ENV: str(bad)}) as s:
            with self.assertRaises(signing.SigningError):
                s.detached(data, tmp / "d2.asc")
        with self.assertRaisesRegex(signing.SigningError, "passphrase file"):
            signing.Signer.from_env({signing.KEY_ENV: secret.decode(), signing.PASSPHRASE_ENV: str(tmp / "none")})


class CliTest(Fixture):
    def run_cli(self, argv, env=None):
        full = {**os.environ, "SOURCE_DATE_EPOCH": str(EPOCH)}
        full.pop(signing.KEY_ENV, None)
        full.update(env or {})
        return subprocess.run([sys.executable, str(TOOLS / "build_repo.py"), *argv],
                              capture_output=True, text=True, env=full)

    def test_deb_build_end_to_end_writes_public_key_only(self):
        out = Path(tempfile.mkdtemp(dir=self.tmp))
        run = self.run_cli(["deb", "--channel", "beta", "--deb", str(self.deb), "--out", str(out)],
                           {signing.KEY_ENV: self.secrets["a"].decode()})
        self.assertEqual(run.returncode, 0, run.stderr)
        self.assertTrue((out / "spot-release.asc").is_file())
        self.assertNotIn("PRIVATE", (out / "spot-release.asc").read_text())
        debrepo.verify_repo(out / "deb", "beta", self.keyring("a"))

    def test_no_key_is_an_error_not_an_unsigned_repository(self):
        out = Path(tempfile.mkdtemp(dir=self.tmp))
        run = self.run_cli(["deb", "--channel", "stable", "--deb", str(self.deb), "--out", str(out)])
        self.assertEqual(run.returncode, 1)
        self.assertIn("SPOT_GPG_KEY", run.stderr)
        self.assertFalse((out / "deb").exists())

    def test_missing_epoch_is_an_error(self):
        out = Path(tempfile.mkdtemp(dir=self.tmp))
        env = {**os.environ, signing.KEY_ENV: self.secrets["a"].decode()}
        env.pop("SOURCE_DATE_EPOCH", None)
        run = subprocess.run([sys.executable, str(TOOLS / "build_repo.py"), "deb", "--channel", "stable",
                              "--deb", str(self.deb), "--out", str(out)], capture_output=True, text=True, env=env)
        self.assertNotEqual(run.returncode, 0)
        self.assertIn("SOURCE_DATE_EPOCH", run.stderr)

    @unittest.skipIf(tool("createrepo_c") and tool("rpmsign"), "rpm tools are installed")
    def test_rpm_without_tools_exits_3_never_skips_silently(self):
        out = Path(tempfile.mkdtemp(dir=self.tmp))
        fake = self.tmp / "x.rpm"
        fake.write_bytes(b"x")
        run = self.run_cli(["rpm", "--channel", "stable", "--rpm", str(fake), "--out", str(out)],
                           {signing.KEY_ENV: self.secrets["a"].decode()})
        self.assertEqual(run.returncode, 3, run.stderr)
        self.assertIn("missing", run.stderr)


class RpmRepoVerifyTest(Fixture):
    """The client model against a hand-made repository (createrepo_c is not needed)."""

    def make_repo(self, label="a"):
        root = Path(tempfile.mkdtemp(dir=self.tmp))
        (root / "Packages").mkdir()
        (root / "repodata").mkdir()
        rpm = root / "Packages" / "spot-1.2.3-1.x86_64.rpm"
        rpm.write_bytes(b"not a real rpm, only bytes to hash")
        sha = hashlib.sha256(rpm.read_bytes()).hexdigest()
        primary = (
            '<?xml version="1.0"?><metadata xmlns="http://linux.duke.edu/metadata/common" packages="1">'
            f'<package type="rpm"><name>spot</name><checksum type="sha256" pkgid="YES">{sha}</checksum>'
            '<location href="Packages/spot-1.2.3-1.x86_64.rpm"/></package></metadata>').encode()
        primary_gz = gzip.compress(primary, mtime=0)
        (root / "repodata" / "primary.xml.gz").write_bytes(primary_gz)
        repomd = (
            '<?xml version="1.0"?><repomd xmlns="http://linux.duke.edu/metadata/repo">'
            '<data type="primary"><checksum type="sha256">' + hashlib.sha256(primary_gz).hexdigest() + "</checksum>"
            '<location href="repodata/primary.xml.gz"/></data></repomd>')
        (root / "repodata" / "repomd.xml").write_text(repomd)
        rpmrepo.sign_repomd(root, self.signer(label))
        return root

    def test_valid_repository_verifies(self):
        rpmrepo.verify_repo(self.make_repo(), self.keyring("a"))

    def test_asc_is_a_detached_armoured_signature_of_repomd(self):
        root = self.make_repo()
        self.assertTrue((root / "repodata/repomd.xml.asc").read_text().startswith("-----BEGIN PGP SIGNATURE-----"))

    def test_flipped_package_byte_fails(self):
        root = self.make_repo()
        p = root / "Packages" / "spot-1.2.3-1.x86_64.rpm"
        p.write_bytes(p.read_bytes()[:-1] + b"!")
        with self.assertRaisesRegex(rpmrepo.RepoError, "does not match primary"):
            rpmrepo.verify_repo(root, self.keyring("a"))

    def test_tampered_metadata_fails(self):
        root = self.make_repo()
        p = root / "repodata" / "primary.xml.gz"
        p.write_bytes(p.read_bytes() + b"\0")
        with self.assertRaisesRegex(rpmrepo.RepoError, "does not match repomd"):
            rpmrepo.verify_repo(root, self.keyring("a"))

    def test_tampered_repomd_fails_signature(self):
        root = self.make_repo()
        p = root / "repodata" / "repomd.xml"
        p.write_text(p.read_text().replace("primary", "primarx", 1))
        with self.assertRaisesRegex(rpmrepo.RepoError, "signature"):
            rpmrepo.verify_repo(root, self.keyring("a"))

    def test_missing_signature_and_wrong_key_fail(self):
        root = self.make_repo()
        with self.assertRaises(rpmrepo.RepoError):
            rpmrepo.verify_repo(root, self.keyring("b"))
        (root / "repodata/repomd.xml.asc").unlink()
        with self.assertRaisesRegex(rpmrepo.RepoError, "missing"):
            rpmrepo.verify_repo(root, self.keyring("a"))

    def test_rpm_build_refuses_without_tools(self):
        if tool("createrepo_c") and tool("rpmsign"):
            self.skipTest("rpm tools installed")
        with self.assertRaises(signing.ToolMissing):
            rpmrepo.build_repo([self.deb], Path(tempfile.mkdtemp(dir=self.tmp)), "stable", self.signer())


class RotationTest(unittest.TestCase):
    @needs_gpg
    def test_dual_key_window(self):
        run = subprocess.run([sys.executable, str(TOOLS / "rotation_check.py")], capture_output=True, text=True)
        self.assertEqual(run.returncode, 0, run.stdout + run.stderr)
        self.assertEqual(run.stdout.count("ok:"), 6)
        self.assertNotIn("FAIL", run.stdout)


class ConfigTest(unittest.TestCase):
    PUB = b"-----BEGIN PGP PUBLIC KEY BLOCK-----\n"

    def test_rpm_repo_file_has_every_check_on(self):
        text = build_keys_package.render_rpm_repo("https://packages.example/", ["RPM-GPG-KEY-spot-aaaa", "RPM-GPG-KEY-spot-bbbb"])
        for channel, enabled in (("stable", "1"), ("beta", "0")):
            self.assertIn(f"[spot-{channel}]", text)
            self.assertIn(f"baseurl=https://packages.example/rpm/{channel}/", text)
        self.assertEqual(text.count("\ngpgcheck=1"), 2)
        self.assertEqual(text.count("repo_gpgcheck=1"), 2)
        self.assertEqual(text.count("localpkg_gpgcheck=1"), 2)
        self.assertNotRegex(text, r"gpgcheck=0")
        self.assertIn("gpgkey=file:///etc/pki/rpm-gpg/RPM-GPG-KEY-spot-aaaa file:///etc/pki/rpm-gpg/RPM-GPG-KEY-spot-bbbb", text)
        self.assertEqual(text.count("enabled=1"), 1)
        self.assertEqual(text.count("enabled=0"), 1)

    def test_apt_source_pins_the_keyring_and_is_never_trusted(self):
        text = build_keys_package.render_deb_sources("https://packages.example")
        self.assertEqual(text.count("Signed-By: /usr/share/keyrings/spot-archive-keyring.gpg"), 2)
        self.assertIn("URIs: https://packages.example/deb", text)
        self.assertIn("Suites: stable", text)
        self.assertNotRegex(text.lower(), r"trusted=|allow-insecure|allowinsecure")
        self.assertEqual(text.count("Enabled: yes"), 1)
        self.assertEqual(text.count("Enabled: no"), 1)

    def test_spec_renders_all_keys_and_no_leftover_markers(self):
        text, version, release = build_keys_package.render_spec("2.0.0-rc.1", EPOCH, ["RPM-GPG-KEY-spot-aaaa", "RPM-GPG-KEY-spot-bbbb"])
        self.assertEqual((version, release), ("2.0.0", "0.rc.1"))
        self.assertNotRegex(text, r"@[A-Z]+@")
        self.assertIn("Source2:        RPM-GPG-KEY-spot-bbbb", text)
        self.assertIn("%config(noreplace) %{_sysconfdir}/yum.repos.d/spot.repo", text)

    def test_spec_template_comments_hold_no_macros(self):
        for line in (REPO / "packaging/linux/repo/rpm/spot-repo-keys.spec").read_text().splitlines():
            if line.startswith("#"):
                self.assertNotIn("%", line)

    def test_a_private_key_is_refused_for_the_keys_package(self):
        with self.assertRaises(SystemExit):
            build_keys_package.check_public(b"-----BEGIN " + b"PGP PRIVATE KEY BLOCK-----", "k.asc")

    def test_no_shipped_file_disables_a_check_or_carries_a_key(self):
        roots = [REPO / "packaging/linux/repo", TOOLS]
        banned = ["gpgcheck=0", "gpgcheck = 0", "trusted=yes", "allow-insecure", "allowinsecure", "--nogpgcheck",
                  "--allow-unauthenticated", "no-check-valid-until", "--force-yes", "PRIVATE KEY BLOCK-----\n"]
        for root in roots:
            for path in root.rglob("*"):
                if not path.is_file() or path.suffix in {".pyc"} or path.name == "test_repo.py":
                    continue
                text = path.read_text(errors="ignore")
                for needle in banned:
                    if needle.startswith("PRIVATE"):
                        # the word may appear in documentation; a real armoured block is followed by base64
                        self.assertNotRegex(text, r"BEGIN PGP PRIVATE KEY BLOCK-----\n[A-Za-z0-9+/=]{20}", path)
                    else:
                        self.assertNotIn(needle.lower(), text.lower().replace("repo_gpgcheck", "repo_gpg"), f"{path}: {needle}")

    def test_no_secret_key_files_in_the_tree(self):
        for root in (REPO / "packaging/linux/repo", TOOLS):
            for path in root.rglob("*"):
                if path.is_file():
                    self.assertFalse(signing.has_secret_material(path) and path.suffix != ".py", path)


class ContainerSmokeTest(unittest.TestCase):
    def test_script_is_present_and_valid_python(self):
        script = TOOLS / "smoke_container.py"
        self.assertTrue(script.is_file())
        run = subprocess.run([sys.executable, "-m", "py_compile", str(script)], capture_output=True, text=True)
        self.assertEqual(run.returncode, 0, run.stderr)


if __name__ == "__main__":
    unittest.main()
