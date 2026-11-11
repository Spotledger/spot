"""Tests for the disposable-host safety gate."""

import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

from agent_linux_harness.orchestrator import (  # noqa: E402
    check_disposable,
    detect_disposable_root,
    detect_runtime,
)
from agent_linux_harness.runner import CommandResult, StubRunner


class DisposableRootTests(unittest.TestCase):
    def test_docker_marker(self):
        runner = StubRunner(files={"/.dockerenv": ""})
        self.assertEqual(detect_disposable_root(runner), (True, "docker"))

    def test_podman_marker(self):
        runner = StubRunner(files={"/run/.containerenv": ""})
        self.assertEqual(detect_disposable_root(runner), (True, "podman"))

    def test_systemd_detect_virt(self):
        runner = StubRunner(
            responses={"systemd-detect-virt": CommandResult(("systemd-detect-virt",), 0, "kvm\n", "")}
        )
        self.assertEqual(detect_disposable_root(runner), (True, "kvm"))

    def test_systemd_detect_virt_none_is_bare_metal(self):
        runner = StubRunner(
            responses={"systemd-detect-virt": CommandResult(("systemd-detect-virt",), 0, "none\n", "")}
        )
        self.assertEqual(detect_disposable_root(runner), (False, "bare metal"))

    def test_cgroup_fallback(self):
        runner = StubRunner(files={"/proc/1/cgroup": "0::/docker/abc\n"})
        self.assertEqual(detect_disposable_root(runner), (True, "docker"))

    def test_plain_host(self):
        runner = StubRunner()
        self.assertEqual(detect_disposable_root(runner), (False, "bare metal"))


class CheckDisposableTests(unittest.TestCase):
    def test_requires_the_environment_variable(self):
        runner = StubRunner(files={"/.dockerenv": ""})
        ok, reason = check_disposable({}, runner)
        self.assertFalse(ok)
        self.assertIn("SPOT_HARNESS_DISPOSABLE", reason)

    def test_requires_a_container_or_vm_root(self):
        ok, reason = check_disposable({"SPOT_HARNESS_DISPOSABLE": "1"}, StubRunner())
        self.assertFalse(ok)
        self.assertIn("not a container or a VM", reason)

    def test_passes_inside_a_container(self):
        runner = StubRunner(files={"/.dockerenv": ""})
        ok, kind = check_disposable({"SPOT_HARNESS_DISPOSABLE": "1"}, runner)
        self.assertTrue(ok)
        self.assertEqual(kind, "docker")


class RuntimeTests(unittest.TestCase):
    def test_prefers_podman(self):
        runner = StubRunner(programs={"podman": "/usr/bin/podman", "docker": "/usr/bin/docker"})
        self.assertEqual(detect_runtime(runner, "auto"), "podman")

    def test_falls_back_to_docker(self):
        runner = StubRunner(programs={"docker": "/usr/bin/docker"})
        self.assertEqual(detect_runtime(runner, "auto"), "docker")

    def test_none_installed(self):
        with self.assertRaises(RuntimeError):
            detect_runtime(StubRunner(), "auto")

    def test_requested_runtime_must_exist(self):
        with self.assertRaises(RuntimeError):
            detect_runtime(StubRunner(), "podman")


if __name__ == "__main__":
    unittest.main()
