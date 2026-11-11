"""Cut drivers: the exact argv lists, the token guard and the disposable-target guard."""

from __future__ import annotations

import unittest

import support  # noqa: F401  (path bootstrap)

from cutdrivers import (
    CutError,
    DiskFreeze,
    ProcessKill,
    ProcessKillWindows,
    VmReset,
    driver_by_name,
    perform_cut,
    safe_token,
)


class StubTarget:
    def __init__(self, pids: list[int], *, disposable: bool = True, vm: str = "windows-vm") -> None:
        self._pids = pids
        self._disposable = disposable
        self.vm_name = vm
        self.device = "/dev/mapper/spot-data"
        self.executed: list[list[str]] = []

    def is_disposable(self) -> bool:
        return self._disposable

    def service_pids(self) -> list[int]:
        return list(self._pids)

    def execute(self, argv, *, timeout: float = 60.0) -> str:
        self.executed.append(list(argv))
        return ""

    def simulate_cut(self, driver_name: str) -> None:  # pragma: no cover - not used here
        self.executed.append(["<simulate>", driver_name])


class ProcessKillTests(unittest.TestCase):
    def test_posix_kills_every_pid(self) -> None:
        target = StubTarget([11, 22])
        plan = ProcessKill().plan(target)
        self.assertEqual(plan, [["kill", "-9", "11"], ["kill", "-9", "22"]])

    def test_no_pids_is_an_error(self) -> None:
        with self.assertRaises(CutError):
            ProcessKill().plan(StubTarget([]))

    def test_pattern_uses_pkill(self) -> None:
        plan = ProcessKill(pattern="spot-server").plan(StubTarget([1]))
        self.assertEqual(plan, [["pkill", "-9", "-f", "spot-server"]])

    def test_unsafe_pattern_is_refused(self) -> None:
        with self.assertRaises(CutError):
            ProcessKill(pattern="spot; rm -rf /").plan(StubTarget([1]))

    def test_windows_uses_taskkill_tree(self) -> None:
        plan = ProcessKillWindows().plan(StubTarget([7]))
        self.assertEqual(plan, [["taskkill", "/F", "/T", "/PID", "7"]])


class VmResetTests(unittest.TestCase):
    def test_libvirt_destroy(self) -> None:
        plan = VmReset(hypervisor="libvirt", vm="windows-vm").plan(StubTarget([1]))
        self.assertEqual(plan, [["virsh", "destroy", "windows-vm"]])

    def test_qemu_monitor_reset(self) -> None:
        plan = VmReset(hypervisor="qemu", vm="windows-vm").plan(StubTarget([1]))
        self.assertEqual(plan, [["virsh", "qemu-monitor-command", "windows-vm", "--hmp", "system_reset"]])

    def test_hyperv_stop_vm(self) -> None:
        plan = VmReset(hypervisor="hyperv", vm="windows-vm").plan(StubTarget([1]))
        self.assertEqual(
            plan,
            [["powershell.exe", "-NoProfile", "-NonInteractive", "-Command", "Stop-VM -Name 'windows-vm' -TurnOff -Force"]],
        )

    def test_vm_name_falls_back_to_target(self) -> None:
        plan = VmReset(hypervisor="libvirt").plan(StubTarget([1], vm="from-target"))
        self.assertEqual(plan[0], ["virsh", "destroy", "from-target"])

    def test_unsafe_vm_name_is_refused(self) -> None:
        with self.assertRaises(CutError):
            VmReset(hypervisor="hyperv", vm="spot'; rm -rf /").plan(StubTarget([1]))

    def test_unknown_hypervisor(self) -> None:
        with self.assertRaises(CutError):
            VmReset(hypervisor="nope", vm="x").plan(StubTarget([1]))

    def test_hyperv_is_windows_only(self) -> None:
        self.assertTrue(VmReset(hypervisor="hyperv").supported("windows"))
        self.assertFalse(VmReset(hypervisor="hyperv").supported("linux"))
        self.assertTrue(VmReset(hypervisor="libvirt").supported("linux"))
        self.assertFalse(VmReset(hypervisor="libvirt").supported("windows"))


class DiskFreezeTests(unittest.TestCase):
    def test_table_names_the_device(self) -> None:
        table = DiskFreeze(device="/dev/sdb", up_interval_ms=0, down_interval_ms=1000).table("/dev/sdb")
        self.assertIn("/dev/sdb", table)
        self.assertIn("flakey", table)

    def test_freeze_plan_suspends_reloads_resumes(self) -> None:
        plan = DiskFreeze(device="/dev/sdb", freeze_ms=500).plan(StubTarget([1]))
        self.assertEqual([argv[1] for argv in plan], ["suspend", "reload", "resume"])

    def test_unsafe_device_is_refused(self) -> None:
        with self.assertRaises(CutError):
            DiskFreeze(device="/dev/sdb; rm").plan(StubTarget([1]))


class GuardTests(unittest.TestCase):
    def test_safe_token_accepts_reasonable_names(self) -> None:
        self.assertEqual(safe_token("spot-win_1.2/x", "vm"), "spot-win_1.2/x")

    def test_perform_cut_refuses_non_disposable(self) -> None:
        with self.assertRaises(CutError):
            perform_cut(ProcessKill(), StubTarget([1], disposable=False), "linux")

    def test_perform_cut_refuses_unsupported_platform(self) -> None:
        with self.assertRaises(CutError):
            perform_cut(VmReset(hypervisor="hyperv", vm="x"), StubTarget([1]), "linux")

    def test_perform_cut_runs_the_plan(self) -> None:
        target = StubTarget([5])
        ran = perform_cut(ProcessKill(), target, "linux")
        self.assertEqual(ran, [["kill", "-9", "5"]])
        self.assertEqual(target.executed, [["kill", "-9", "5"]])

    def test_unknown_driver_name(self) -> None:
        with self.assertRaises(CutError):
            driver_by_name("teleport")


if __name__ == "__main__":
    unittest.main()
