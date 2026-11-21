"""Every command line is an argument list, never a shell string.

The one thing that must never happen is a package path being handed to a shell: a name with
a space or a semicolon would then split into extra arguments or run a second command. These
tests assert the shape of the argv builders directly, and with a hostile path.
"""

import json
import unittest

import support  # noqa: F401 - puts the driver modules on sys.path

import commands
import distros

#: A path that would do real damage if it were ever parsed by a shell.
HOSTILE = "/run/spot-pkgs/spot 1.2.3; touch /tmp/pwned.rpm"


class ArgvShapeTests(unittest.TestCase):
    def test_every_builder_returns_a_list_of_strings(self):
        f = distros.FEDORA_44
        u = distros.UBUNTU_2404
        builders = [
            commands.install(HOSTILE, f),
            commands.install(HOSTILE, u),
            commands.remove(f),
            commands.remove(u),
            commands.purge(f),
            commands.purge(u),
            commands.is_installed(f),
            commands.installed_version(u),
            commands.config_files(f),
            commands.unit_show("MainPID"),
            commands.start(),
            commands.stop(),
            commands.is_enabled(),
            commands.is_active(),
            commands.verify_unit(f.unit_path),
            commands.kill("123"),
            commands.health(f.health_url),
            commands.stat("%a", f.config_path),
            commands.getent("passwd", "spot"),
            commands.cat(f.config_path),
            commands.exists(f.config_path),
            commands.install_file("/w/a", "/e/b", mode="0640", owner="root", group="spot"),
            commands.id_user(),
        ]
        for argv in builders:
            with self.subTest(argv=argv):
                self.assertIsInstance(argv, list)
                self.assertTrue(argv)
                self.assertTrue(all(isinstance(item, str) for item in argv))

    def test_a_hostile_package_path_stays_one_argument(self):
        for distro in distros.DISTROS:
            argv = commands.install(HOSTILE, distro)
            self.assertEqual(argv[-1], HOSTILE)
            self.assertNotIn("sh", argv)
            self.assertNotIn("-c", argv)
            self.assertNotIn("bash", argv)
            # The semicolon produced no extra argument of its own.
            self.assertEqual(len([item for item in argv if item.startswith("touch")]), 0)
            # Round-tripping through JSON keeps it one element, so a log or a report
            # cannot turn it into a command either.
            self.assertEqual(json.loads(json.dumps(argv))[-1], HOSTILE)

    def test_apt_is_non_interactive_and_skips_recommends(self):
        argv = commands.install("/pkg/spot_1.2.3_amd64.deb", distros.UBUNTU_2404)
        self.assertEqual(argv[:2], ["env", "DEBIAN_FRONTEND=noninteractive"])
        self.assertIn("--no-install-recommends", argv)
        self.assertEqual(argv[-1], "/pkg/spot_1.2.3_amd64.deb")

    def test_dnf_install_of_a_local_file(self):
        argv = commands.install("/pkg/spot-1.2.3-1.fc44.x86_64.rpm", distros.FEDORA_44)
        self.assertEqual(argv, ["dnf", "-y", "install", "/pkg/spot-1.2.3-1.fc44.x86_64.rpm"])

    def test_unit_show_passes_one_property_per_flag(self):
        argv = commands.unit_show("ActiveState", "MainPID")
        self.assertEqual(
            argv,
            ["systemctl", "show", "-p", "ActiveState", "-p", "MainPID", "spot.service"],
        )


class MetadataParsingTests(unittest.TestCase):
    def test_rpm_noreplace_flag(self):
        stdout = "/usr/bin/spot \n/etc/spot/spot.toml cn\n/etc/spot/environment cn\n"
        self.assertTrue(
            commands.config_is_noreplace(distros.FEDORA_44, stdout, "/etc/spot/spot.toml")
        )
        self.assertFalse(
            commands.config_is_noreplace(distros.FEDORA_44, stdout, "/etc/spot/other.toml")
        )

    def test_rpm_config_without_the_noreplace_flag(self):
        stdout = "/etc/spot/spot.toml c\n"
        self.assertFalse(
            commands.config_is_noreplace(distros.FEDORA_44, stdout, "/etc/spot/spot.toml")
        )

    def test_deb_conffile(self):
        stdout = "\n /etc/spot/spot.toml 0123456789abcdef0123456789abcdef\n"
        self.assertTrue(
            commands.config_is_noreplace(distros.UBUNTU_2404, stdout, "/etc/spot/spot.toml")
        )
        self.assertFalse(
            commands.config_is_noreplace(distros.UBUNTU_2404, stdout, "/etc/spot/other")
        )

    def test_parse_show(self):
        parsed = commands.parse_show("ActiveState=active\nRestart=on-failure\n\nMainPID=42\n")
        self.assertEqual(parsed["ActiveState"], "active")
        self.assertEqual(parsed["Restart"], "on-failure")
        self.assertEqual(parsed["MainPID"], "42")

    def test_parse_microseconds(self):
        self.assertEqual(commands.parse_microseconds("40000000"), 40)
        self.assertEqual(commands.parse_microseconds("0"), 0)
        self.assertEqual(commands.parse_microseconds(""), 0)
        self.assertEqual(commands.parse_microseconds("infinity"), 0)

    def test_login_shell(self):
        self.assertTrue(commands.login_shell_is_disabled("/sbin/nologin"))
        self.assertTrue(commands.login_shell_is_disabled("/usr/sbin/nologin"))
        self.assertFalse(commands.login_shell_is_disabled("/bin/bash"))

    def test_dpkg_config_files_state_is_not_installed(self):
        self.assertFalse(
            commands.package_is_installed(distros.UBUNTU_2404, "deinstall ok config-files")
        )
        self.assertFalse(
            commands.package_is_installed(distros.UBUNTU_2404, "unknown ok not-installed")
        )
        self.assertTrue(
            commands.package_is_installed(distros.UBUNTU_2404, "install ok installed")
        )

    def test_rpm_absence_is_empty_output(self):
        self.assertFalse(commands.package_is_installed(distros.FEDORA_44, ""))
        self.assertTrue(
            commands.package_is_installed(distros.FEDORA_44, "spot-1.2.3-1.fc44.x86_64")
        )

    def test_passwd_fields(self):
        fields = commands.passwd_fields(
            "spot:x:998:996:Spotledger service:/var/lib/spot:/sbin/nologin\n"
        )
        self.assertEqual(len(fields), 7)
        self.assertEqual(fields[0], "spot")
        self.assertEqual(fields[5], "/var/lib/spot")
        self.assertEqual(fields[6], "/sbin/nologin")


if __name__ == "__main__":
    unittest.main()
