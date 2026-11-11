"""One test class per scenario, driving it against a scripted runner.

Each scenario is proved twice: once on the happy path, where every scripted answer is what
the distribution really gives, and once on the assertion that matters, which has to fail.
No container is started and no package is installed: the runner is the seam.
"""

import dataclasses
import tempfile
import unittest
from pathlib import Path

import support  # noqa: F401 - puts the driver modules on sys.path

import distros
import scenario_container
import scenario_downgrade
import scenario_install
import scenario_removal
import scenario_service
import scenario_upgrade
from context import ScenarioFailure, ScenarioSkip

FEDORA = distros.FEDORA_44
UBUNTU = distros.UBUNTU_2404


class ScenarioTestCase(unittest.TestCase):
    """A temporary fixture directory, removed after each test."""

    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory(prefix="spot-verify-scenario-")
        self.work = Path(self._tmp.name)

    def tearDown(self):
        self._tmp.cleanup()


class CleanInstallTests(ScenarioTestCase):
    def _scripts(self, stub, *, shell="/sbin/nologin", enabled=(1, "disabled\n"),
                 active=(3, "inactive\n")):
        stub.add(("dnf", "-y", "install"))
        stub.add(
            ("getent", "passwd", "spot"),
            stdout=f"spot:x:998:996:Spotledger service:/var/lib/spot:{shell}\n",
        )
        stub.add(("getent", "group", "spot"), stdout="spot:x:996:\n")
        stub.add(
            ("stat", "-c", "%a %U %G %n", "/etc/spot/spot.toml"),
            stdout="640 root spot /etc/spot/spot.toml\n",
        )
        stub.add(
            ("rpm", "-q", "--qf"),
            stdout="/usr/bin/spot \n/etc/spot/spot.toml cn\n/etc/spot/environment cn\n",
        )
        stub.add(
            ("stat", "-c", "%a %n", "/usr/lib/systemd/system/spot.service"),
            stdout="644 /usr/lib/systemd/system/spot.service\n",
        )
        stub.add(("systemd-analyze", "verify"))
        stub.add(
            ("systemctl", "is-enabled", "spot.service"),
            returncode=enabled[0],
            stdout=enabled[1],
        )
        stub.add(
            ("systemctl", "is-active", "spot.service"),
            returncode=active[0],
            stdout=active[1],
        )
        stub.add(
            ("stat", "-c", "%a %U %G %n", "/var/lib/spot"),
            stdout="750 spot spot /var/lib/spot\n",
        )

    def test_every_promise_holds(self):
        context, stub, _ = support.context_for(FEDORA, (support.rpm("1.2.3"),), work_dir=self.work)
        self._scripts(stub)
        detail = scenario_install.run(context)
        self.assertIn("installed", detail)
        installs = [call for call in stub.calls if "dnf" in call]
        self.assertEqual(len(installs), 1)
        self.assertEqual(installs[0][-1], "/run/spot-pkgs/spot-1.2.3-1.fc44.x86_64.rpm")

    def test_a_package_that_enables_the_service_fails_the_scenario(self):
        context, stub, _ = support.context_for(FEDORA, (support.rpm("1.2.3"),), work_dir=self.work)
        self._scripts(stub, enabled=(0, "enabled\n"))
        with self.assertRaises(ScenarioFailure) as raised:
            scenario_install.run(context)
        self.assertIn("enabled the service", str(raised.exception))

    def test_a_package_that_starts_the_service_fails_the_scenario(self):
        context, stub, _ = support.context_for(FEDORA, (support.rpm("1.2.3"),), work_dir=self.work)
        self._scripts(stub, active=(0, "active\n"))
        with self.assertRaises(ScenarioFailure) as raised:
            scenario_install.run(context)
        self.assertIn("started the service", str(raised.exception))

    def test_a_login_shell_fails_the_scenario(self):
        context, stub, _ = support.context_for(FEDORA, (support.rpm("1.2.3"),), work_dir=self.work)
        self._scripts(stub, shell="/bin/bash")
        with self.assertRaises(ScenarioFailure) as raised:
            scenario_install.run(context)
        self.assertIn("login shell", str(raised.exception))

    def test_without_an_artefact_it_skips(self):
        context, _, _ = support.context_for(FEDORA, (), work_dir=self.work)
        with self.assertRaises(ScenarioSkip):
            scenario_install.run(context)

    def test_a_hostile_package_name_reaches_the_runner_as_one_argument(self):
        artefact = support.rpm("1.2.3", filename="spot 1.2.3; touch x.rpm")
        context, stub, _ = support.context_for(FEDORA, (artefact,), work_dir=self.work)
        self._scripts(stub)
        scenario_install.run(context)
        install = [call for call in stub.calls if "dnf" in call][0]
        self.assertEqual(install[-1], "/run/spot-pkgs/spot 1.2.3; touch x.rpm")
        self.assertFalse(any(item in ("sh", "bash", "-c") for item in install))


class StartAndHealthTests(ScenarioTestCase):
    def _scripts(self, stub, *, restart="on-failure", new_pid="200", sticky_pair=False):
        stub.add(("systemctl", "start", "spot.service"))
        stub.add(("curl", "-fsS"))
        stub.add(
            ("systemctl", "show", "-p", "ActiveState", "-p", "Restart", "-p", "MainPID"),
            stdout=f"ActiveState=active\nRestart={restart}\nMainPID=100\n",
        )
        stub.add(("kill", "-9", "100"))
        stub.add(
            ("systemctl", "show", "-p", "ActiveState", "-p", "MainPID"),
            stdout=f"ActiveState=active\nMainPID={new_pid}\n",
            sticky=sticky_pair,
        )
        stub.add(("systemctl", "show", "-p", "MainPID"), stdout=f"MainPID={new_pid}\n")
        stub.add(
            ("systemctl", "show", "-p", "TimeoutStopUSec"), stdout="TimeoutStopUSec=40000000\n"
        )
        stub.add(("systemctl", "stop", "spot.service"))

    def test_health_restart_and_bounded_stop(self):
        context, stub, _ = support.context_for(FEDORA, (support.rpm("1.2.3"),), work_dir=self.work)
        self._scripts(stub)
        detail = scenario_service.run(context)
        self.assertIn("healthy after", detail)
        self.assertIn("100", detail)
        self.assertIn("200", detail)
        self.assertIn("stop took", detail)
        self.assertIn("of 40 s", detail)

    def test_a_health_endpoint_that_never_answers_fails(self):
        context, stub, _ = support.context_for(
            FEDORA,
            (support.rpm("1.2.3"),),
            work_dir=self.work,
            clock=support.TickingClock(step_ms=7000),
        )
        stub.add(("systemctl", "start", "spot.service"))
        stub.add(("curl", "-fsS"), returncode=7, stderr="connection refused", sticky=True)
        with self.assertRaises(ScenarioFailure) as raised:
            scenario_service.run(context)
        self.assertIn("health endpoint", str(raised.exception))

    def test_a_restart_policy_that_is_not_on_failure_fails(self):
        context, stub, _ = support.context_for(FEDORA, (support.rpm("1.2.3"),), work_dir=self.work)
        self._scripts(stub, restart="no")
        with self.assertRaises(ScenarioFailure) as raised:
            scenario_service.run(context)
        self.assertIn("Restart=", str(raised.exception))

    def test_a_process_that_is_not_restarted_fails(self):
        context, stub, _ = support.context_for(
            FEDORA,
            (support.rpm("1.2.3"),),
            work_dir=self.work,
            clock=support.TickingClock(step_ms=11000),
        )
        self._scripts(stub, new_pid="100", sticky_pair=True)
        with self.assertRaises(ScenarioFailure) as raised:
            scenario_service.run(context)
        self.assertIn("restarted main process", str(raised.exception))


class UpgradeTests(ScenarioTestCase):
    def _scripts(self, stub, edit_text, *, new_pid="200", sticky_pair=False,
                 settled_pid=None):
        stub.add(("rpm", "-q", "--qf", "%{VERSION}-%{RELEASE}"), stdout="1.2.3-1")
        stub.add(("install", "-m", "0640", "-o", "spot", "-g", "spot"))
        stub.add(("/run/spot-verify/spot.toml.edited",))
        stub.add(("systemctl", "start", "spot.service"))
        stub.add(("curl", "-fsS"))
        stub.add(("systemctl", "show", "-p", "MainPID"), stdout="MainPID=100\n")
        stub.add(("dnf", "-y", "install", "/run/spot-pkgs/spot-1.2.4-1.fc44.x86_64.rpm"))
        stub.add(("rpm", "-q", "--qf", "%{VERSION}-%{RELEASE}"), stdout="1.2.4-1")
        stub.add(("cat", "/etc/spot/spot.toml"), stdout=edit_text)
        stub.add(("cat", "/var/lib/spot/verify-marker"), stdout=scenario_upgrade.MARKER_CONTENT)
        stub.add(("test", "-e", "/etc/spot/spot.toml.rpmnew"), returncode=1)
        pair = f"ActiveState=active\nMainPID={new_pid}\n"
        stub.add(
            ("systemctl", "show", "-p", "ActiveState", "-p", "MainPID"),
            stdout=pair,
            sticky=sticky_pair,
        )
        stub.add(("systemctl", "show", "-p", "MainPID"), stdout=f"MainPID={new_pid}\n")
        stub.add(
            ("systemctl", "show", "-p", "ActiveState", "-p", "MainPID"),
            stdout=f"ActiveState=active\nMainPID={settled_pid or new_pid}\n",
        )

    def test_config_state_and_a_single_restart(self):
        artefacts = (support.rpm("1.2.3"), support.rpm("1.2.4"))
        context, stub, _ = support.context_for(FEDORA, artefacts, work_dir=self.work)
        self._scripts(stub, scenario_upgrade._edited_config_text())
        detail = scenario_upgrade.run(context)
        self.assertIn("restarted once", detail)
        self.assertIn("edited config kept", detail)
        self.assertIn("state marker intact", detail)
        self.assertIn("1.2.3 -> 1.2.4", detail)
        self.assertTrue((self.work / scenario_upgrade.MARKER_NAME).exists())

    def test_an_overwritten_config_fails_the_scenario(self):
        artefacts = (support.rpm("1.2.3"), support.rpm("1.2.4"))
        context, stub, _ = support.context_for(FEDORA, artefacts, work_dir=self.work)
        self._scripts(stub, "# the packaged template, not the edit\n")
        with self.assertRaises(ScenarioFailure) as raised:
            scenario_upgrade.run(context)
        self.assertIn("overwrote the edited config", str(raised.exception))

    def test_a_service_that_does_not_come_back_fails(self):
        artefacts = (support.rpm("1.2.3"), support.rpm("1.2.4"))
        context, stub, _ = support.context_for(
            FEDORA, artefacts, work_dir=self.work, clock=support.TickingClock(step_ms=11000)
        )
        self._scripts(stub, scenario_upgrade._edited_config_text(), new_pid="100", sticky_pair=True)
        with self.assertRaises(ScenarioFailure) as raised:
            scenario_upgrade.run(context)
        self.assertIn("restarted service", str(raised.exception))

    def test_a_second_restart_fails_the_scenario(self):
        artefacts = (support.rpm("1.2.3"), support.rpm("1.2.4"))
        context, stub, _ = support.context_for(FEDORA, artefacts, work_dir=self.work)
        self._scripts(stub, scenario_upgrade._edited_config_text(), settled_pid="300")
        with self.assertRaises(ScenarioFailure) as raised:
            scenario_upgrade.run(context)
        self.assertIn("more than once", str(raised.exception))

    def _deb_scripts(self, stub, *, new_pid="200", settled_pid=None, sticky_pair=False):
        # The postinst runs inside the apt-get call below: it makes `systemctl daemon-reload`
        # and then `systemctl try-restart spot.service` on the machine (see
        # tools/packaging/tests/test_maintainer_scripts.py). The harness itself issues neither;
        # it only watches the service's MainPID afterwards, which is what is scripted here.
        stub.add(("dpkg-query", "-W", "-f"), stdout="1.2.3")
        stub.add(("install", "-m", "0640", "-o", "spot", "-g", "spot"))
        stub.add(("/run/spot-verify/spot.toml.edited",))
        stub.add(("systemctl", "start", "spot.service"))
        stub.add(("curl", "-fsS"))
        stub.add(("systemctl", "show", "-p", "MainPID"), stdout="MainPID=100\n")
        stub.add(
            ("apt-get", "-y", "--no-install-recommends", "install",
             "/run/spot-pkgs/spot_1.2.4_amd64.deb")
        )
        stub.add(("dpkg-query", "-W", "-f"), stdout="1.2.4")
        stub.add(("cat", "/etc/spot/spot.toml"), stdout=scenario_upgrade._edited_config_text())
        stub.add(("cat", "/var/lib/spot/verify-marker"), stdout=scenario_upgrade.MARKER_CONTENT)
        stub.add(("test", "-e", "/etc/spot/spot.toml.dpkg-dist"), returncode=1)
        stub.add(
            ("systemctl", "show", "-p", "ActiveState", "-p", "MainPID"),
            stdout=f"ActiveState=active\nMainPID={new_pid}\n",
            sticky=sticky_pair,
        )
        stub.add(("systemctl", "show", "-p", "MainPID"), stdout=f"MainPID={new_pid}\n")
        stub.add(
            ("systemctl", "show", "-p", "ActiveState", "-p", "MainPID"),
            stdout=f"ActiveState=active\nMainPID={settled_pid or new_pid}\n",
        )

    def test_the_deb_policy_expects_one_restart(self):
        self.assertTrue(distros.UBUNTU_POLICY.restart_on_upgrade)
        artefacts = (support.deb("1.2.3"), support.deb("1.2.4"))
        context, stub, _ = support.context_for(UBUNTU, artefacts, work_dir=self.work)
        self._deb_scripts(stub)
        detail = scenario_upgrade.run(context)
        self.assertIn("restarted once", detail)
        self.assertIn("100 -> 200", detail)
        self.assertIn("dpkg-dist absent", detail)
        # The reload and the restart belong to the package, not to the harness: a harness that
        # issued either would hide a postinst that forgot it.
        issued = [" ".join(call) for call in stub.calls]
        self.assertFalse(any("daemon-reload" in call for call in issued), issued)
        self.assertFalse(any("try-restart" in call for call in issued), issued)

    def test_a_deb_upgrade_that_does_not_restart_fails(self):
        artefacts = (support.deb("1.2.3"), support.deb("1.2.4"))
        context, stub, _ = support.context_for(
            UBUNTU, artefacts, work_dir=self.work, clock=support.TickingClock(step_ms=11000)
        )
        self._deb_scripts(stub, new_pid="100", sticky_pair=True)
        with self.assertRaises(ScenarioFailure) as raised:
            scenario_upgrade.run(context)
        self.assertIn("restarted service", str(raised.exception))

    def test_a_second_deb_restart_fails_the_scenario(self):
        artefacts = (support.deb("1.2.3"), support.deb("1.2.4"))
        context, stub, _ = support.context_for(UBUNTU, artefacts, work_dir=self.work)
        self._deb_scripts(stub, settled_pid="300")
        with self.assertRaises(ScenarioFailure) as raised:
            scenario_upgrade.run(context)
        self.assertIn("more than once", str(raised.exception))

    def test_a_restart_where_the_policy_says_none_fails(self):
        # A copy of the distro with a no-restart policy: the shared constants stay untouched.
        no_restart = dataclasses.replace(distros.UBUNTU_POLICY, restart_on_upgrade=False)
        distro = dataclasses.replace(distros.UBUNTU_2404, policy=no_restart)
        self.assertTrue(distros.UBUNTU_POLICY.restart_on_upgrade)
        artefacts = (support.deb("1.2.3"), support.deb("1.2.4"))
        context, stub, _ = support.context_for(distro, artefacts, work_dir=self.work)
        stub.add(("dpkg-query", "-W", "-f"), stdout="1.2.3")
        stub.add(("install", "-m", "0640", "-o", "spot", "-g", "spot"))
        stub.add(("/run/spot-verify/spot.toml.edited",))
        stub.add(("systemctl", "start", "spot.service"))
        stub.add(("curl", "-fsS"))
        stub.add(("systemctl", "show", "-p", "MainPID"), stdout="MainPID=100\n")
        stub.add(
            ("apt-get", "-y", "--no-install-recommends", "install",
             "/run/spot-pkgs/spot_1.2.4_amd64.deb")
        )
        stub.add(("dpkg-query", "-W", "-f"), stdout="1.2.4")
        stub.add(("cat", "/etc/spot/spot.toml"), stdout=scenario_upgrade._edited_config_text())
        stub.add(("cat", "/var/lib/spot/verify-marker"), stdout=scenario_upgrade.MARKER_CONTENT)
        stub.add(("test", "-e", "/etc/spot/spot.toml.dpkg-dist"), returncode=1)
        stub.add(
            ("systemctl", "show", "-p", "ActiveState", "-p", "MainPID"),
            stdout="ActiveState=active\nMainPID=200\n",
        )
        with self.assertRaises(ScenarioFailure) as raised:
            scenario_upgrade.run(context)
        self.assertIn("policy in distros.py is wrong", str(raised.exception))

    def test_without_two_versions_it_skips(self):
        context, _, _ = support.context_for(FEDORA, (support.rpm("1.2.4"),), work_dir=self.work)
        with self.assertRaises(ScenarioSkip):
            scenario_upgrade.run(context)


class DowngradeTests(ScenarioTestCase):
    def test_rpm_leaves_the_newer_version_installed(self):
        artefacts = (support.rpm("1.2.3"), support.rpm("1.2.4"))
        context, stub, _ = support.context_for(FEDORA, artefacts, work_dir=self.work)
        stub.add(("rpm", "-q", "--qf"), stdout="1.2.4-1")
        stub.add(("dnf", "-y", "install"), returncode=1, stderr="Error: already installed")
        stub.add(("rpm", "-q", "--qf"), stdout="1.2.4-1")
        detail = scenario_downgrade.run(context)
        self.assertIn("left 1.2.4-1 installed", detail)
        self.assertIn("exited 1", detail)

    def test_deb_records_the_refusal(self):
        artefacts = (support.deb("1.2.3"), support.deb("1.2.4"))
        context, stub, _ = support.context_for(UBUNTU, artefacts, work_dir=self.work)
        stub.add(("dpkg-query", "-W", "-f"), stdout="1.2.4")
        stub.add(("apt-get", "-y"), returncode=100, stderr="E: downgrade not allowed")
        stub.add(("dpkg-query", "-W", "-f"), stdout="1.2.4")
        detail = scenario_downgrade.run(context)
        self.assertIn("exited 100", detail)

    def test_a_silent_downgrade_fails(self):
        artefacts = (support.rpm("1.2.3"), support.rpm("1.2.4"))
        context, stub, _ = support.context_for(FEDORA, artefacts, work_dir=self.work)
        stub.add(("rpm", "-q", "--qf"), stdout="1.2.4-1")
        stub.add(("dnf", "-y", "install"))
        stub.add(("rpm", "-q", "--qf"), stdout="1.2.3-1")
        with self.assertRaises(ScenarioFailure) as raised:
            scenario_downgrade.run(context)
        self.assertIn("silent downgrade", str(raised.exception))

    def test_without_two_versions_it_skips(self):
        context, _, _ = support.context_for(FEDORA, (support.rpm("1.2.4"),), work_dir=self.work)
        with self.assertRaises(ScenarioSkip):
            scenario_downgrade.run(context)


class RemovalTests(ScenarioTestCase):
    def _canaries(self, stub, distro):
        stub.add(("/run/spot-verify/spot.toml.edited",))
        stub.add(
            (
                "install", "-m", "0644", "-o", "root", "-g", "root",
                "/run/spot-verify/canary-verify-canary", "/etc/spot/verify-canary",
            )
        )
        stub.add(
            (
                "install", "-m", "0640", "-o", distro.user, "-g", distro.group,
                "/run/spot-verify/canary-verify-canary", f"{distro.data_dir}/verify-canary",
            )
        )
        stub.add(
            (
                "install", "-m", "0640", "-o", distro.user, "-g", distro.group,
                "/run/spot-verify/canary-verify-data", f"{distro.data_dir}/verify-data",
            )
        )

    def test_rpm_erase_keeps_the_data_and_saves_the_config(self):
        context, stub, _ = support.context_for(FEDORA, (support.rpm("1.2.4"),), work_dir=self.work)
        stub.add(("rpm", "-q", "--qf"), stdout="1.2.4-1")
        self._canaries(stub, FEDORA)
        stub.add(("dnf", "-y", "remove", "spot"))
        stub.add(("test", "-e", "/usr/bin/spot"), returncode=1)
        stub.add(("test", "-e", FEDORA.unit_path), returncode=1)
        stub.add(("rpm", "-q", "spot"), returncode=1, stderr="package spot is not installed")
        stub.add(("test", "-e", "/etc/spot/spot.toml"), returncode=1)
        stub.add(("test", "-e", "/etc/spot/spot.toml.rpmsave"))
        stub.add(("cat", "/var/lib/spot/verify-data"), stdout=scenario_removal.DATA_CONTENT)
        stub.add(("test", "-e", "/var/lib/spot/verify-canary"))
        stub.add(("getent", "passwd", "spot"), stdout="spot:x:998:996::/var/lib/spot:/sbin/nologin\n")
        detail = scenario_removal.run(context)
        self.assertIn("rpmsave", detail)
        self.assertNotIn("purged", detail)

    def test_a_removal_that_deletes_the_data_fails(self):
        context, stub, _ = support.context_for(FEDORA, (support.rpm("1.2.4"),), work_dir=self.work)
        stub.add(("rpm", "-q", "--qf"), stdout="1.2.4-1")
        self._canaries(stub, FEDORA)
        stub.add(("dnf", "-y", "remove", "spot"))
        stub.add(("test", "-e", "/usr/bin/spot"), returncode=1)
        stub.add(("test", "-e", FEDORA.unit_path), returncode=1)
        stub.add(("rpm", "-q", "spot"), returncode=1)
        stub.add(("test", "-e", "/etc/spot/spot.toml"), returncode=1)
        stub.add(("test", "-e", "/etc/spot/spot.toml.rpmsave"))
        stub.add(("cat", "/var/lib/spot/verify-data"), returncode=1, stderr="No such file")
        with self.assertRaises(ScenarioFailure) as raised:
            scenario_removal.run(context)
        self.assertIn("state marker", str(raised.exception))

    def test_deb_remove_keeps_the_config_and_purge_removes_it(self):
        context, stub, _ = support.context_for(UBUNTU, (support.deb("1.2.3"),), work_dir=self.work)
        stub.add(("dpkg-query", "-W", "-f"), stdout="1.2.3")
        self._canaries(stub, UBUNTU)
        stub.add(("apt-get", "-y", "remove", "spot"))
        stub.add(("test", "-e", "/usr/bin/spot"), returncode=1)
        stub.add(("test", "-e", UBUNTU.unit_path), returncode=1)
        stub.add(("dpkg-query", "-W", "-f", "${Status}"), stdout="deinstall ok config-files")
        stub.add(("test", "-e", "/etc/spot/spot.toml"))
        stub.add(("cat", "/var/lib/spot/verify-data"), stdout=scenario_removal.DATA_CONTENT)
        stub.add(("test", "-e", "/var/lib/spot/verify-canary"))
        stub.add(("getent", "passwd", "spot"), stdout="spot:x:998:996::/var/lib/spot:/sbin/nologin\n")
        stub.add(("apt-get", "-y", "purge", "spot"))
        stub.add(("test", "-e", "/etc/spot/spot.toml"), returncode=1)
        stub.add(("test", "-e", "/etc/spot/verify-canary"))
        stub.add(("cat", "/var/lib/spot/verify-data"), stdout=scenario_removal.DATA_CONTENT)
        stub.add(("test", "-e", "/var/lib/spot/verify-canary"))
        detail = scenario_removal.run(context)
        self.assertIn("purged", detail)

    def test_purge_deleting_a_foreign_canary_fails(self):
        context, stub, _ = support.context_for(UBUNTU, (support.deb("1.2.3"),), work_dir=self.work)
        stub.add(("dpkg-query", "-W", "-f"), stdout="1.2.3")
        self._canaries(stub, UBUNTU)
        stub.add(("apt-get", "-y", "remove", "spot"))
        stub.add(("test", "-e", "/usr/bin/spot"), returncode=1)
        stub.add(("test", "-e", UBUNTU.unit_path), returncode=1)
        stub.add(("dpkg-query", "-W", "-f", "${Status}"), stdout="deinstall ok config-files")
        stub.add(("test", "-e", "/etc/spot/spot.toml"))
        stub.add(("cat", "/var/lib/spot/verify-data"), stdout=scenario_removal.DATA_CONTENT)
        stub.add(("test", "-e", "/var/lib/spot/verify-canary"))
        stub.add(("getent", "passwd", "spot"), stdout="spot:x:998:996::/var/lib/spot:/sbin/nologin\n")
        stub.add(("apt-get", "-y", "purge", "spot"))
        stub.add(("test", "-e", "/etc/spot/spot.toml"), returncode=1)
        stub.add(("test", "-e", "/etc/spot/verify-canary"), returncode=1)
        with self.assertRaises(ScenarioFailure) as raised:
            scenario_removal.run(context)
        self.assertIn("did not install", str(raised.exception))

    def test_removal_without_an_installed_package_fails(self):
        context, stub, _ = support.context_for(FEDORA, (support.rpm("1.2.4"),), work_dir=self.work)
        stub.add(("rpm", "-q", "--qf"), returncode=1)
        with self.assertRaises(ScenarioFailure) as raised:
            scenario_removal.run(context)
        self.assertIn("not installed", str(raised.exception))


class ContainerImageTests(ScenarioTestCase):
    def _binary(self) -> Path:
        binary = self.work / "spot-binary"
        binary.write_bytes(b"\x7fELF not really an elf\n")
        return binary

    def _scripts(self, stub, *, health=("healthy\n", False), writable=False, stop_ok=True,
                 exit_code="0\n"):
        stub.add(("podman", "build"))
        stub.add(("podman", "run", "-d"))
        stub.add(("podman", "exec", scenario_container.CONTAINER_NAME, "id", "-u"), stdout="998\n")
        stub.add(("podman", "inspect", "--format", "{{.Config.User}}"), stdout="spot:spot\n")
        stub.add(
            ("podman", "inspect", "--format", "{{.State.Health.Status}}"),
            stdout=health[0],
            sticky=health[1],
        )
        stub.add(
            ("podman", "exec", scenario_container.CONTAINER_NAME, "touch", "/etc/verify-probe"),
            returncode=0 if writable else 1,
            stderr="" if writable else "Read-only file system",
        )
        stub.add(
            ("podman", "stop", "--time", "30"),
            returncode=0 if stop_ok else 1,
            stderr="" if stop_ok else "no such container",
        )
        stub.add(("podman", "inspect", "--format", "{{.State.ExitCode}}"), stdout=exit_code)
        stub.add(("podman", "rm", "-f"))

    def test_the_image_runs_non_root_read_only_and_stops_cleanly(self):
        context, stub, _ = support.context_for(
            FEDORA, (support.rpm("1.2.4"),), work_dir=self.work, binary=self._binary()
        )
        self._scripts(stub)
        detail = scenario_container.run(context)
        self.assertIn("uid 998", detail)
        self.assertIn("healthy", detail)
        self.assertIn("SIGTERM exit 0", detail)
        build = [call for call in stub.calls if "build" in call][0]
        self.assertIn("VERSION=1.2.4", build)
        run = [call for call in stub.calls if "run" in call][0]
        self.assertIn("--read-only", run)

    def test_a_failing_healthcheck_fails_the_scenario(self):
        context, stub, _ = support.context_for(
            FEDORA,
            (support.rpm("1.2.4"),),
            work_dir=self.work,
            binary=self._binary(),
            clock=support.TickingClock(step_ms=31000),
        )
        self._scripts(stub, health=("unhealthy\n", True))
        with self.assertRaises(ScenarioFailure) as raised:
            scenario_container.run(context)
        self.assertIn("HEALTHCHECK", str(raised.exception))

    def test_a_writable_root_filesystem_fails_the_scenario(self):
        context, stub, _ = support.context_for(
            FEDORA, (support.rpm("1.2.4"),), work_dir=self.work, binary=self._binary()
        )
        self._scripts(stub, writable=True)
        with self.assertRaises(ScenarioFailure) as raised:
            scenario_container.run(context)
        self.assertIn("writable", str(raised.exception))

    def test_a_non_zero_exit_on_sigterm_fails_the_scenario(self):
        context, stub, _ = support.context_for(
            FEDORA, (support.rpm("1.2.4"),), work_dir=self.work, binary=self._binary()
        )
        self._scripts(stub, exit_code="143\n")
        with self.assertRaises(ScenarioFailure) as raised:
            scenario_container.run(context)
        self.assertIn("SIGTERM", str(raised.exception))

    def test_without_a_binary_it_skips(self):
        context, _, _ = support.context_for(FEDORA, (), work_dir=self.work)
        with self.assertRaises(ScenarioSkip):
            scenario_container.run(context)

    def test_the_container_is_always_removed(self):
        context, stub, _ = support.context_for(
            FEDORA, (support.rpm("1.2.4"),), work_dir=self.work, binary=self._binary()
        )
        self._scripts(stub, stop_ok=False)
        with self.assertRaises(ScenarioFailure):
            scenario_container.run(context)
        self.assertTrue(any("rm" in call for call in stub.calls))


if __name__ == "__main__":
    unittest.main()
