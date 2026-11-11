"""Tests for tools/docsite/bootstrap.py (WP-DX-04.T1).

The dep bootstrap must never touch the network here: every subprocess and
exec call is mocked, and the tests assert the shape of the fallback ladder —
venv first with a re-exec, pip --user second with an in-process re-probe,
and a full diagnostic listing every attempt when both fail.
"""
from __future__ import annotations

import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

HERE = Path(__file__).resolve().parent
DOCSITE = HERE.parent
sys.path.insert(0, str(DOCSITE))

import bootstrap  # noqa: E402  (stdlib only; safe to import without the pins)


def done(returncode=0, stdout="", stderr=""):
    return subprocess.CompletedProcess(args=["mocked"], returncode=returncode,
                                       stdout=stdout, stderr=stderr)


class Missing(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.venv = Path(self.tmp.name) / "venv"

    def run_ladder(self, *, run_results, missing_rounds, exec_ok=True):
        """Drive ensure_deps with the whole subprocess surface mocked.

        run_results: per-call results for bootstrap.subprocess.run (last one
        repeats). missing_rounds: per-call results for bootstrap.missing.
        Returns (raised_exit_text_or_None, execv_call_or_None, run_calls).
        """
        calls: list = []

        def fake_run(cmd, *a, **kw):
            calls.append(cmd)
            return run_results[min(len(calls) - 1, len(run_results) - 1)]

        execv: list = []

        def fake_execv(path, argv):
            execv.append((path, argv))
            if not exec_ok:
                raise OSError("execv failed")

        env = {k: v for k, v in os.environ.items()
               if k not in ("SPOT_DOCSITE_REEXEC", "PYTHONNOUSERSITE")}
        with mock.patch.object(bootstrap, "missing", side_effect=missing_rounds), \
             mock.patch.object(bootstrap, "venv_dir", return_value=self.venv), \
             mock.patch.object(bootstrap.subprocess, "run", side_effect=fake_run), \
             mock.patch.object(bootstrap.os, "execv", side_effect=fake_execv), \
             mock.patch.dict(bootstrap.os.environ, env, clear=True):
            text = None
            try:
                bootstrap.ensure_deps()
            except SystemExit as exc:
                text = str(exc)
        return text, (execv[0] if execv else None), calls


class EnsureDepsTests(Missing):
    def setUp(self):
        super().setUp()
        # when CI's real bootstrap re-execs into the venv, it leaves
        # SPOT_DOCSITE_REEXEC=1 in os.environ; these tests drive ensure_deps
        # directly, so each starts from a clean guard
        self._guard = os.environ.pop("SPOT_DOCSITE_REEXEC", None)

    def tearDown(self):
        if self._guard is not None:
            os.environ["SPOT_DOCSITE_REEXEC"] = self._guard
        super().tearDown()

    def test_all_pins_present_short_circuits(self):
        with mock.patch.object(bootstrap, "missing", return_value=[]), \
             mock.patch.object(bootstrap.subprocess, "run") as run, \
             mock.patch.object(bootstrap.os, "execv") as execv:
            bootstrap.ensure_deps()
        run.assert_not_called()
        execv.assert_not_called()

    def test_requirements_file_missing_names_the_path(self):
        with mock.patch.object(bootstrap, "REQUIREMENTS",
                               Path(self.tmp.name) / "nope.txt"), \
             mock.patch.object(bootstrap, "missing", return_value=["yaml"]):
            with self.assertRaises(SystemExit) as ctx:
                bootstrap.ensure_deps()
        self.assertIn("nope.txt", str(ctx.exception))

    def test_reexec_guard_refuses_to_loop(self):
        with mock.patch.object(bootstrap, "missing", return_value=["yaml"]), \
             mock.patch.dict(os.environ, {"SPOT_DOCSITE_REEXEC": "1"}):
            with self.assertRaises(SystemExit) as ctx:
                bootstrap.ensure_deps()
        self.assertIn("refusing to loop", str(ctx.exception))

    def test_venv_install_success_reexecs_the_command(self):
        py = bootstrap._python(self.venv)
        text, exec_call, calls = self.run_ladder(
            run_results=[done(), done(), done()],  # venv create, pip, probe
            missing_rounds=[["yaml"]])
        self.assertIsNone(text)
        self.assertIsNotNone(exec_call)
        assert exec_call is not None
        self.assertEqual(exec_call[0], str(py))
        # the throwaway venv is outside the repository
        self.assertNotIn(str(DOCSITE), str(self.venv))
        # the pins are installed into the venv, no --quiet hiding the errors
        self.assertEqual(calls[1][:3], [str(py), "-m", "pip"])
        self.assertNotIn("--quiet", calls[1])

    def test_windows_venv_layout(self):
        with mock.patch.object(bootstrap.os, "name", "nt"):
            self.assertTrue(str(bootstrap._python(self.venv))
                            .endswith("Scripts/python.exe"))
        with mock.patch.object(bootstrap.os, "name", "posix"):
            self.assertTrue(str(bootstrap._python(self.venv))
                            .endswith("bin/python"))

    def test_user_fallback_when_venv_install_fails(self):
        pip_fail = done(returncode=1, stderr="network unreachable")
        text, exec_call, calls = self.run_ladder(
            run_results=[done(), pip_fail, done(returncode=0)],
            missing_rounds=[["yaml"], []])  # in-process re-probe after --user succeeds
        self.assertIsNone(text)  # ensure_deps returned: imports now available
        self.assertIsNone(exec_call)  # --user path needs no re-exec
        self.assertEqual(calls[-1][:4],
                         [sys.executable, "-m", "pip", "install"])
        self.assertIn("--user", calls[-1])

    def test_no_pip_detected_and_both_fails_list_attempts(self):
        no_pip = done(returncode=1, stderr="/x/bin/python: No module named pip")
        user_fail = done(returncode=1, stderr="No module named pip")
        text, exec_call, calls = self.run_ladder(
            run_results=[done(), no_pip, user_fail],
            missing_rounds=[["yaml"], ["yaml"], ["yaml"], ["yaml"]])
        self.assertIsNotNone(text)
        self.assertIsNone(exec_call)
        assert text is not None
        self.assertIn("Attempts:", text)
        self.assertIn("1.", text)
        self.assertIn("2.", text)
        self.assertIn("no pip", text)
        self.assertIn("venv", text)

    def test_user_fallback_skipped_when_nousersite_set(self):
        pip_fail = done(returncode=1, stderr="network unreachable")
        env = {k: v for k, v in os.environ.items() if k != "SPOT_DOCSITE_REEXEC"}
        env["PYTHONNOUSERSITE"] = "1"
        with mock.patch.object(bootstrap, "missing", return_value=["yaml"]), \
             mock.patch.object(bootstrap, "venv_dir", return_value=self.venv), \
             mock.patch.object(bootstrap.subprocess, "run",
                               side_effect=[done(), pip_fail]), \
             mock.patch.dict(bootstrap.os.environ, env, clear=True):
            with self.assertRaises(SystemExit) as ctx:
                bootstrap.ensure_deps()
        self.assertIn("PYTHONNOUSERSITE", str(ctx.exception))

    def test_failed_attempt_output_is_echoed_not_hidden(self):
        # --quiet was the reason CI logs showed nothing; _run must echo the
        # captured pip output when an attempt fails.
        import io
        pip_fail = done(returncode=1,
                        stderr="ERROR: Could not find a version that satisfies "
                               "the requirement PyYAML==6.0.3")
        buf = io.StringIO()
        with mock.patch.object(bootstrap.subprocess, "run",
                               return_value=pip_fail), \
             mock.patch.object(bootstrap.sys, "stderr", buf):
            proc = bootstrap._run(["python", "-m", "pip", "install",
                                   "-r", "requirements.txt"])
        self.assertEqual(proc.returncode, 1)
        self.assertIn("Could not find a version", buf.getvalue())


class TargetTests(unittest.TestCase):
    def test_plain_script_command(self):
        with mock.patch.object(sys, "argv", ["build.py", "--out", "x"]):
            t = bootstrap._target(Path("/venv/bin/python"))
        self.assertEqual(t, ["/venv/bin/python", "build.py", "--out", "x"])

    def test_unittest_module_command(self):
        with mock.patch.object(sys, "argv",
                               ["/py/unittest/__main__.py", "discover", "-s",
                                "tools/docsite/tests"]):
            with mock.patch.object(bootstrap, "_parent", return_value="unittest"):
                t = bootstrap._target(Path("/venv/bin/python"))
        self.assertEqual(t[0], "/venv/bin/python")
        self.assertEqual(t[1:3], ["-m", "unittest"])
        self.assertEqual(t[3:], ["discover", "-s", "tools/docsite/tests"])


if __name__ == "__main__":
    unittest.main()
