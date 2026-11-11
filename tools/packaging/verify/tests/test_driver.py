"""The driver: the container argv, the systemd wait and how results are recorded.

The container is never started here. A stub runner answers the podman calls, so the tests
assert the exact argv the driver builds (``--systemd=always``, one mount per element) and how
a setup failure, a skipped prerequisite and a failed scenario each reach the report.
"""

import shutil
import tempfile
import unittest
from pathlib import Path

import support  # noqa: F401 - puts the driver modules on sys.path

import driver
import report
import scenarios
import distros

FEDORA = distros.FEDORA_44


def silent(*_args, **_kwargs) -> None:
    """An ``emit`` that prints nothing, so the tests stay quiet."""


class ContainerArgvTests(unittest.TestCase):
    def test_create_argv_uses_systemd_and_one_mount_per_element(self):
        package_dir = support.Path("/tmp/spot pkgs; rm -rf dist")
        work_dir = support.Path("/tmp/work dir")
        argv = driver.container_create_argv("podman", FEDORA, package_dir, work_dir)
        self.assertEqual(argv[:4], ["podman", "run", "-d", "--name"])
        self.assertIn("--systemd=always", argv)
        self.assertIn(f"{package_dir}:/run/spot-pkgs:ro,z", argv)
        self.assertIn(f"{work_dir}:/run/spot-verify:rw,z", argv)
        self.assertEqual(argv[-1], FEDORA.image_tag)
        self.assertNotIn("sh", argv)

    def test_create_argv_without_a_package_directory_has_no_package_mount(self):
        argv = driver.container_create_argv("podman", FEDORA, None, None)
        self.assertFalse(any("/run/spot-pkgs" in item for item in argv))

    def test_build_argv_points_at_the_harness_containerfile(self):
        argv = driver.image_build_argv("podman", FEDORA)
        self.assertEqual(argv[:3], ["podman", "build", "--format"])
        self.assertIn(str(FEDORA.containerfile), argv)
        self.assertIn(FEDORA.image_tag, argv)

    def test_remove_argv_is_forced(self):
        self.assertEqual(
            driver.container_remove_argv("podman", FEDORA),
            ["podman", "rm", "-f", FEDORA.container_name],
        )


class DriverRunTests(unittest.TestCase):
    def _driver(self, stub):
        return driver.Driver(
            stub,
            support.TickingClock(),
            lambda _seconds: None,
            runtime="podman",
            package_dir=None,
            binary=None,
            emit=silent,
        )

    def _start_scripts(self, stub):
        stub.add(("podman", "build"))
        stub.add(("podman", "run", "-d"))
        stub.add(("systemctl", "is-system-running"), stdout="running\n")

    def test_a_failed_scenario_is_recorded_with_its_log(self):
        stub = support.StubRunner()
        self._start_scripts(stub)
        stub.add(("dnf", "-y", "install"), returncode=1, stderr="conflicting requests")
        stub.add(("podman", "rm", "-f"))
        directory = Path(tempfile.mkdtemp(prefix="spot-verify-driver-"))
        support.write_packages(directory, [support.RPM_OLD])
        instance = driver.Driver(
            stub,
            support.TickingClock(),
            lambda _seconds: None,
            runtime="podman",
            package_dir=directory,
            emit=silent,
        )
        try:
            rows = instance.run((FEDORA,), (scenarios.get("clean_install"),))
        finally:
            instance.close()
            shutil.rmtree(directory, ignore_errors=True)
        self.assertEqual(len(rows), 1)
        self.assertEqual(rows[0].status, report.FAIL)
        self.assertIn("conflicting requests", rows[0].detail)
        self.assertTrue(rows[0].log)
        self.assertTrue(any("rm" in call for call in stub.calls))

    def test_a_scenario_whose_prerequisite_did_not_run_is_skipped(self):
        stub = support.StubRunner()
        self._start_scripts(stub)
        stub.add(("podman", "rm", "-f"))
        instance = self._driver(stub)
        try:
            rows = instance.run((FEDORA,), (scenarios.get("start_and_health"),))
        finally:
            instance.close()
        self.assertEqual(rows[0].status, report.SKIP)
        self.assertIn("needs clean_install", rows[0].detail)

    def test_a_failed_image_build_marks_every_row_an_error(self):
        stub = support.StubRunner()
        stub.add(("podman", "build"), returncode=1, stderr="no such base image")
        instance = self._driver(stub)
        try:
            rows = instance.run((FEDORA,), (scenarios.get("clean_install"),))
        finally:
            instance.close()
        self.assertEqual(rows[0].status, report.ERROR)
        self.assertIn("could not build", rows[0].detail)
        self.assertTrue(any("no such base image" in line for line in rows[0].log))

    def test_a_container_that_does_not_start_is_an_error(self):
        stub = support.StubRunner()
        stub.add(("podman", "build"))
        stub.add(("podman", "run", "-d"), returncode=125, stderr="cgroup v2 required")
        instance = self._driver(stub)
        try:
            rows = instance.run((FEDORA,), (scenarios.get("clean_install"),))
        finally:
            instance.close()
        self.assertEqual(rows[0].status, report.ERROR)
        self.assertIn("could not start", rows[0].detail)
        self.assertFalse(any(call[1:2] == ("rm",) for call in stub.calls))

    def test_systemd_that_never_gets_ready_is_an_error(self):
        stub = support.StubRunner()
        stub.add(("podman", "build"))
        stub.add(("podman", "run", "-d"))
        stub.add(("systemctl", "is-system-running"), returncode=1, stdout="starting\n", sticky=True)
        instance = driver.Driver(
            stub,
            support.TickingClock(step_ms=60000),
            lambda _seconds: None,
            runtime="podman",
            package_dir=None,
            emit=silent,
        )
        try:
            rows = instance.run((FEDORA,), (scenarios.get("clean_install"),))
        finally:
            instance.close()
        self.assertEqual(rows[0].status, report.ERROR)
        self.assertIn("systemd did not become ready", rows[0].detail)

    def test_keep_stops_the_removal_and_the_cleanup(self):
        stub = support.StubRunner()
        self._start_scripts(stub)
        stub.add(("rpm", "-q", "--qf"), returncode=1)
        instance = driver.Driver(
            stub,
            support.TickingClock(),
            lambda _seconds: None,
            runtime="podman",
            package_dir=None,
            keep=True,
            emit=silent,
        )
        work_dir = instance.work_dir
        try:
            instance.run((FEDORA,), (scenarios.get("removal"),))
        finally:
            instance.close()
        self.assertFalse(any(call[1:2] == ("rm",) for call in stub.calls))
        self.assertTrue(work_dir.exists())

    def test_the_shared_scenario_runs_once_and_is_reported_once(self):
        stub = support.StubRunner()
        instance = self._driver(stub)
        try:
            rows = instance.run((FEDORA, distros.ALMALINUX_10), (scenarios.get("container_image"),))
        finally:
            instance.close()
        self.assertEqual(len(rows), 1)
        self.assertEqual(rows[0].distro, report.SHARED_DISTRO)
        self.assertEqual(rows[0].status, report.SKIP)
        self.assertIn("no release binary", rows[0].detail)


if __name__ == "__main__":
    unittest.main()
