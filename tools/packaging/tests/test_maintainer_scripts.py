"""Shell-level tests for the .deb maintainer scripts.

The scripts run as root on an operator's machine, so they are run for real, not
read: every external command they may call (``systemctl``, ``systemd-sysusers``,
``systemd-tmpfiles``, ``getent``, ``chgrp``) is replaced by a recorder placed on
a private ``PATH`` that is the only entry. The recorder appends its own name and
arguments to a log file, so the tests assert the exact set of calls the package
makes to the machine's service manager.

The shipped source package installs the systemd unit but never starts or enables
it. The only service actions are one ``daemon-reload`` followed by one
``try-restart`` when an upgrade replaces the package (``configure`` with a
non-empty previous version) and systemd is running.
"""

import os
import re
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import build  # noqa: E402

DEBIAN = build.PACKAGING / "debian"
SCRIPTS = ("postinst", "prerm", "postrm")

#: The one path the scripts use to decide whether systemd is the init system.
SYSTEMD_RUN_DIR = "/run/systemd/system"

#: Appends ``name arg arg`` to ``$SPOT_TEST_CALLS`` and succeeds, unless the call is
#: exactly ``$SPOT_TEST_FAIL`` (``name arg arg``), which is recorded and then fails.
RECORDER = (
    "#!/bin/sh\n"
    "printf '%s' \"${0##*/}\" >> \"$SPOT_TEST_CALLS\"\n"
    "for arg in \"$@\"; do\n"
    "\tprintf ' %s' \"$arg\" >> \"$SPOT_TEST_CALLS\"\n"
    "done\n"
    "printf '\\n' >> \"$SPOT_TEST_CALLS\"\n"
    "if [ \"${0##*/} $*\" = \"${SPOT_TEST_FAIL:-}\" ]; then exit 1; fi\n"
    "exit 0\n"
)

#: Every command the scripts may call, recorded instead of run.
RECORDED = ("systemctl", "systemd-sysusers", "systemd-tmpfiles", "getent", "chgrp")

#: A real ``systemctl`` invocation in command position (not ``command -v systemctl``).
SYSTEMCTL_CALL = re.compile(r"(?:^|[;&|])\s*systemctl\s+([a-z][a-z-]*)")


def systemctl_calls(text):
    """Yield ``(line, verb)`` for every ``systemctl`` call in a script's text."""
    for line in text.splitlines():
        if line.lstrip().startswith("#"):
            continue
        for match in SYSTEMCTL_CALL.finditer(line):
            yield line, match.group(1)


class MaintainerScriptTest(unittest.TestCase):
    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory(prefix="spot-maintainer-")
        self.tmp = Path(self._tmp.name)
        self.stubs = self.tmp / "stub"
        self.stubs.mkdir()
        for name in RECORDED:
            stub = self.stubs / name
            stub.write_text(RECORDER, encoding="ascii")
            stub.chmod(0o755)
        self.calls = self.tmp / "calls"
        self.systemd_run_dir = self.tmp / "run-systemd" / "system"

    def tearDown(self):
        self._tmp.cleanup()

    def _install_script(self, name, systemd_running):
        """Copy a shipped script, pointing its systemd guard at a directory we own."""
        text = (DEBIAN / name).read_text(encoding="ascii")
        if SYSTEMD_RUN_DIR in text:
            self.assertEqual(
                text.count(SYSTEMD_RUN_DIR), 1,
                f"{name} must test {SYSTEMD_RUN_DIR} once, not {text.count(SYSTEMD_RUN_DIR)} times",
            )
            guard = self.systemd_run_dir if systemd_running else self.tmp / "absent"
            if systemd_running:
                guard.mkdir(parents=True, exist_ok=True)
            text = text.replace(SYSTEMD_RUN_DIR, str(guard))
        path = self.tmp / name
        path.write_text(text, encoding="ascii")
        path.chmod(0o755)
        return path

    def run_script(self, name, *args, systemd_running=True, fail=None):
        """Run a maintainer script exactly as dpkg would, and return its calls.

        ``fail`` names one recorded call (``"systemctl daemon-reload"``) that exits non-zero.
        """
        path = self._install_script(name, systemd_running)
        self.calls.unlink(missing_ok=True)
        environment = {"PATH": str(self.stubs), "SPOT_TEST_CALLS": str(self.calls), "LC_ALL": "C"}
        if fail:
            environment["SPOT_TEST_FAIL"] = fail
        run = subprocess.run([str(path), *args], cwd=str(self.tmp), env=environment,
                             capture_output=True, text=True, check=False)
        self.assertEqual(
            run.returncode, 0,
            f"{name} {' '.join(args)} exited {run.returncode}: {run.stdout}{run.stderr}",
        )
        if not self.calls.exists():
            return []
        return [line for line in self.calls.read_text(encoding="ascii").splitlines() if line]

    @staticmethod
    def service_manager_calls(calls):
        return [line for line in calls if line.split(" ", 1)[0] == "systemctl"]

    def test_fresh_install_starts_nothing(self):
        calls = self.run_script("postinst", "configure")
        self.assertEqual(self.service_manager_calls(calls), [])
        # The account and the directories are still created from the shipped data.
        self.assertIn("systemd-sysusers spot.conf", calls)
        self.assertIn("systemd-tmpfiles --create spot.conf", calls)

    def test_fresh_install_with_an_empty_previous_version_starts_nothing(self):
        calls = self.run_script("postinst", "configure", "")
        self.assertEqual(self.service_manager_calls(calls), [])

    UPGRADE_CALLS = ["systemctl daemon-reload", "systemctl try-restart spot.service"]

    def test_upgrade_reloads_systemd_then_restarts_a_running_service(self):
        calls = self.run_script("postinst", "configure", "1.2.2")
        self.assertEqual(self.service_manager_calls(calls), self.UPGRADE_CALLS)

    def test_upgrade_asks_for_no_other_service_action(self):
        calls = self.service_manager_calls(self.run_script("postinst", "configure", "1.2.2"))
        self.assertEqual(
            [c for c in calls if c == "systemctl try-restart spot.service"],
            ["systemctl try-restart spot.service"],
            "exactly one try-restart",
        )
        self.assertEqual(calls.count("systemctl daemon-reload"), 1, "exactly one daemon-reload")
        self.assertEqual(len(calls), 2, f"the upgrade must call systemctl twice, not {calls}")

    def test_a_failing_daemon_reload_neither_skips_the_restart_nor_fails_dpkg(self):
        # run_script asserts the exit status is 0.
        calls = self.run_script("postinst", "configure", "1.2.2", fail="systemctl daemon-reload")
        self.assertEqual(self.service_manager_calls(calls), self.UPGRADE_CALLS)

    def test_a_failing_restart_does_not_fail_dpkg(self):
        calls = self.run_script(
            "postinst", "configure", "1.2.2", fail="systemctl try-restart spot.service"
        )
        self.assertEqual(self.service_manager_calls(calls), self.UPGRADE_CALLS)

    def test_upgrade_without_a_running_systemd_calls_nothing(self):
        calls = self.run_script("postinst", "configure", "1.2.2", systemd_running=False)
        self.assertEqual(self.service_manager_calls(calls), [])

    def test_a_downgrade_is_an_upgrade_for_this_purpose(self):
        calls = self.run_script("postinst", "configure", "2.0.0")
        self.assertEqual(self.service_manager_calls(calls), self.UPGRADE_CALLS)

    def test_other_dpkg_actions_do_nothing(self):
        for action in ("abort-upgrade", "abort-remove", "abort-deconfigure", "triggered"):
            with self.subTest(action=action):
                self.assertEqual(self.run_script("postinst", action, "1.2.2"), [])

    def test_prerm_stops_and_restarts_nothing(self):
        for action in ("remove", "upgrade", "deconfigure", "failed-upgrade"):
            with self.subTest(action=action):
                self.assertEqual(self.run_script("prerm", action, "1.2.2"), [])

    def test_postrm_never_starts_the_service(self):
        for action in ("remove", "purge", "upgrade", "failed-upgrade", "abort-install"):
            with self.subTest(action=action):
                calls = self.run_script("postrm", action, "1.2.2")
                self.assertEqual(calls, [], f"postrm {action} must run no command at all")

    def test_no_script_starts_stops_or_enables_the_service(self):
        # A static reading of the shipped text, so a future edit that adds
        # "systemctl start" to prerm fails here even if it is guarded.
        for name in SCRIPTS:
            text = (DEBIAN / name).read_text(encoding="ascii")
            for line, verb in systemctl_calls(text):
                with self.subTest(script=name, line=line.strip()):
                    self.assertIn(verb, ("daemon-reload", "try-restart"),
                                  f"{name} may only daemon-reload and try-restart, not {verb}")
        for name in ("prerm", "postrm"):
            with self.subTest(script=name):
                self.assertEqual(list(systemctl_calls((DEBIAN / name).read_text(encoding="ascii"))), [])
                self.assertNotIn("systemctl ", (DEBIAN / name).read_text(encoding="ascii"))

    def test_the_restart_names_the_unit_literally(self):
        # Nothing from the environment may reach the service manager: no "$" on
        # the lines that call systemctl.
        text = (DEBIAN / "postinst").read_text(encoding="ascii")
        calls = list(systemctl_calls(text))
        self.assertEqual([verb for _, verb in calls], ["daemon-reload", "try-restart"])
        for line, _ in calls:
            self.assertNotIn("$", line)
        self.assertIn("try-restart spot.service", calls[1][0])

    def test_the_reload_comes_before_the_restart_in_the_same_guard(self):
        text = (DEBIAN / "postinst").read_text(encoding="ascii")
        reload_at = text.index("systemctl daemon-reload")
        restart_at = text.index("systemctl try-restart spot.service")
        self.assertLess(reload_at, restart_at)
        guard = text.rindex("if [ -n", 0, reload_at)
        self.assertNotIn("\nfi\n", text[guard:restart_at], "both calls sit in one guarded branch")
        self.assertIn("|| true", text.splitlines()[text[:reload_at].count("\n")])

    OPTIONS = ("--no-restart-after-upgrade", "--no-stop-on-upgrade")

    def test_the_debhelper_path_cannot_add_a_second_restart(self):
        # The postinst owns the one upgrade restart. debian/rules is the dh build path, where
        # dh_installsystemd generates its own restart snippets; when the override exists it
        # must carry both no-restart options or neither (never half), and it must keep the
        # promise that the package starts and enables nothing.
        text = (DEBIAN / "rules").read_text(encoding="ascii")
        match = re.search(r"^override_dh_installsystemd:\n((?:\t.*\n)+)", text, re.MULTILINE)
        if match is None:
            self.skipTest("debian/rules has no override_dh_installsystemd")
        recipe = match.group(1)
        self.assertIn("--no-start", recipe)
        self.assertIn("--no-enable", recipe)
        present = [option for option in self.OPTIONS if option in recipe]
        self.assertIn(len(present), (0, len(self.OPTIONS)),
                      f"override_dh_installsystemd carries only {present} of {self.OPTIONS}")

    def test_the_scripts_are_posix_sh(self):
        for name in SCRIPTS:
            path = DEBIAN / name
            text = path.read_text(encoding="ascii")
            with self.subTest(script=name):
                self.assertTrue(text.startswith("#!/bin/sh\n"), "the shebang must be /bin/sh")
                for bashism in ("[[", "function ", "local ", "$'", "=="):
                    self.assertNotIn(bashism, text, f"{name} uses the bashism {bashism!r}")
                parsed = subprocess.run(["sh", "-n", str(path)], capture_output=True, text=True)
                self.assertEqual(parsed.returncode, 0, parsed.stderr)

    def test_the_scripts_are_executable(self):
        for name in SCRIPTS:
            with self.subTest(script=name):
                self.assertTrue(os.access(DEBIAN / name, os.X_OK), f"{name} must be executable")

    def test_the_scripts_do_not_read_the_environment(self):
        for name in SCRIPTS:
            text = (DEBIAN / name).read_text(encoding="ascii")
            with self.subTest(script=name):
                self.assertNotIn("${SPOT", text)
                self.assertNotIn("getenv", text)


if __name__ == "__main__":
    unittest.main()
