import re
import sys
import tarfile
import tempfile
import unittest
from pathlib import Path
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import build  # noqa: E402
import check  # noqa: E402

PACKAGING = Path(__file__).resolve().parents[3] / "packaging" / "linux"


def read(path: Path) -> str:
    return path.read_text(encoding="utf-8")


class UnitLintTest(unittest.TestCase):
    def setUp(self):
        self.text = read(PACKAGING / "systemd" / "spot.service")

    def assertRejected(self, text, needle, label):
        errors = check.lint_unit(text)
        self.assertTrue(errors, f"{label}: expected errors, got none")
        self.assertTrue(any(needle in error for error in errors),
                        f"{label}: no error mentioned {needle!r}: {errors}")

    def test_shipped_unit_passes_builtin_lint(self):
        self.assertEqual(check.lint_unit(self.text), [])

    def test_check_unit_without_systemd_analyze(self):
        result = check.check_unit(PACKAGING / "systemd" / "spot.service",
                                  use_systemd_analyze=False)
        self.assertTrue(result["ok"], result)
        self.assertEqual(result["errors"], [])

    def test_rejects_mutated_units(self):
        mutations = [
            ("missing NoNewPrivileges", "NoNewPrivileges=true\n", "", "NoNewPrivileges"),
            ("User=root", "User=spot", "User=root", "User=root"),
            ("ProtectSystem=full", "ProtectSystem=strict", "ProtectSystem=full", "ProtectSystem"),
            ("writable /etc", "ReadWritePaths=/var/lib/spot", "ReadWritePaths=/etc", "ReadWritePaths"),
            ("missing SystemCallFilter", "SystemCallFilter=@system-service\n", "", "SystemCallFilter"),
            ("capabilities granted", "CapabilityBoundingSet=\n", "CapabilityBoundingSet=CAP_SYS_ADMIN\n",
             "CapabilityBoundingSet"),
            ("missing PrivateDevices", "PrivateDevices=true\n", "", "PrivateDevices"),
            ("extra address family", "RestrictAddressFamilies=AF_UNIX AF_INET AF_INET6",
             "RestrictAddressFamilies=AF_UNIX AF_INET AF_INET6 AF_PACKET", "RestrictAddressFamilies"),
            ("MemoryDenyWriteExecute", "LockPersonality=true", "LockPersonality=true\nMemoryDenyWriteExecute=true",
             "MemoryDenyWriteExecute"),
            ("missing RestrictNamespaces", "RestrictNamespaces=true\n", "", "RestrictNamespaces"),
            ("missing TimeoutStopSec", "TimeoutStopSec=40s\n", "", "TimeoutStopSec"),
        ]
        for label, old, new, needle in mutations:
            with self.subTest(label=label):
                self.assertIn(old, self.text)
                self.assertRejected(self.text.replace(old, new, 1), needle, label)

    @unittest.skipUnless(check.systemd_available(), "systemd-analyze not installed")
    def test_systemd_analyze_accepts_the_unit(self):
        result = check.verify_unit(PACKAGING / "systemd" / "spot.service")
        self.assertTrue(result["ok"], result)
        self.assertLessEqual(result["security_score"], check.SECURITY_THRESHOLD)


class TarballCheckTest(unittest.TestCase):
    def test_built_tarball_passes_check(self):
        with tempfile.TemporaryDirectory(prefix="spot-check-test-") as tmp:
            root = Path(tmp)
            binary = root / "spot"
            binary.write_bytes(b"#!/bin/sh\nexit 0\n")
            binary.chmod(0o755)
            result = build.build("tarball", binary, "1.2.3", root / "out", "x86_64")
            checked = check.check_tarball(Path(result["artifact"]))
            self.assertTrue(checked["ok"], checked)
            self.assertEqual(checked["errors"], [])


class MetadataTest(unittest.TestCase):
    def test_spec_has_required_structure(self):
        spec = read(PACKAGING / "rpm" / "spot.spec")
        for tag in ("Name:", "Version:", "Release:", "Summary:", "License:", "Source0:",
                    "%install", "%files", "%changelog", "%pre", "%post", "%preun", "%postun"):
            self.assertIn(tag, spec, tag)
        for macro in ("%systemd_post spot.service", "%systemd_preun spot.service",
                      "%systemd_postun_with_restart spot.service",
                      "%sysusers_create_compat", "%config(noreplace)", "%ghost", "%dir"):
            self.assertIn(macro, spec, macro)
        self.assertNotIn("cargo", spec.lower())
        self.assertNotIn("systemctl start", spec.lower())
        self.assertNotIn("systemctl enable", spec.lower())

    def test_spec_post_does_not_start_the_service(self):
        spec = read(PACKAGING / "rpm" / "spot.spec")
        post = re.search(r"(?ms)^%post\n(.*?)^%(?:preun|postun)\b", spec)
        self.assertIsNotNone(post)
        body = post.group(1).lower()
        self.assertNotIn("start", body)
        self.assertNotIn("enable", body)
        self.assertIn("%systemd_post", body)

    def test_debian_metadata(self):
        control = read(PACKAGING / "debian" / "control")
        for tag in ("Source:", "Package:", "Architecture:", "Depends:", "Maintainer:",
                    "Section:", "Priority:", "Description:"):
            self.assertIn(tag, control, tag)
        rules = read(PACKAGING / "debian" / "rules")
        self.assertTrue(rules.startswith("#!/usr/bin/make -f"))
        self.assertIn("dh_installsystemd", rules)
        self.assertIn("--no-start", rules)
        conffiles = read(PACKAGING / "debian" / "conffiles").split()
        self.assertEqual(conffiles, ["/etc/spot/spot.toml", "/etc/spot/environment"])
        for script in ("postinst", "prerm", "postrm"):
            body = read(PACKAGING / "debian" / script).lower()
            self.assertNotIn("systemctl start", body, script)
            self.assertNotIn("service spot start", body, script)

    def test_containerfile_structure_is_valid(self):
        result = check.check_containerfile(PACKAGING / "container" / "Containerfile")
        self.assertTrue(result["ok"], result)
        text = read(PACKAGING / "container" / "Containerfile")
        self.assertIn("STOPSIGNAL SIGTERM", text)
        self.assertIn("USER spot:spot", text)
        self.assertIn('VOLUME ["/var/lib/spot"]', text)

    def test_package_checks_skip_without_tools(self):
        with mock.patch.object(check.shutil, "which", return_value=None):
            rpm = check.check_rpm(Path("does-not-exist.rpm"))
            deb = check.check_deb(Path("does-not-exist.deb"))
            container = check.check_container("spot:0.0.0")
        self.assertTrue(rpm["skipped"])
        self.assertTrue(deb["skipped"])
        self.assertTrue(container["skipped"])


class TarballLayoutTest(unittest.TestCase):
    def test_config_file_is_shipped_with_safe_defaults(self):
        text = read(PACKAGING / "etc" / "spot.toml")
        self.assertIn('bind = ["127.0.0.1:8000"]', text)
        self.assertIn("shutdown_grace = 30", text)
        self.assertNotIn("bind = [\"0.0.0.0", text)


if __name__ == "__main__":
    unittest.main()
