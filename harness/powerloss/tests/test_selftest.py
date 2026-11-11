"""The CLI: the ``--selftest`` flag, the cycle command and the driver listing."""

from __future__ import annotations

import io
import unittest
from contextlib import redirect_stdout

import support  # noqa: F401  (path bootstrap)

import powerloss


class SelftestTests(unittest.TestCase):
    def test_selftest_flag_passes(self) -> None:
        original = powerloss.SELFTEST_CYCLES
        powerloss.SELFTEST_CYCLES = 4
        try:
            buffer = io.StringIO()
            with redirect_stdout(buffer):
                code = powerloss.main(["--selftest", "--seed", "5"])
            output = buffer.getvalue()
        finally:
            powerloss.SELFTEST_CYCLES = original
        self.assertEqual(code, 0, output)
        self.assertIn("SELFTEST OK", output)
        self.assertIn("negative control", output)

    def test_selftest_subcommand_passes(self) -> None:
        original = powerloss.SELFTEST_CYCLES
        powerloss.SELFTEST_CYCLES = 3
        try:
            with redirect_stdout(io.StringIO()):
                code = powerloss.main(["selftest", "--seed", "9"])
        finally:
            powerloss.SELFTEST_CYCLES = original
        self.assertEqual(code, 0)


class CommandTests(unittest.TestCase):
    def test_drivers_lists_every_driver(self) -> None:
        buffer = io.StringIO()
        with redirect_stdout(buffer):
            code = powerloss.main(["drivers"])
        self.assertEqual(code, 0)
        for name in ("process-kill", "process-kill-windows", "vm-reset", "disk-freeze"):
            self.assertIn(name, buffer.getvalue())

    def test_cycle_passes_on_the_model(self) -> None:
        buffer = io.StringIO()
        with redirect_stdout(buffer):
            code = powerloss.main(["cycle", "--seed", "2", "--cut-after", "10", "--operations", "20", "--workers", "1"])
        self.assertEqual(code, 0, buffer.getvalue())
        self.assertIn("powerloss invariants: OK", buffer.getvalue())

    def test_cycle_with_unsafe_settings_reports_violations(self) -> None:
        buffer = io.StringIO()
        with redirect_stdout(buffer):
            code = powerloss.main(
                [
                    "cycle",
                    "--seed", "2",
                    "--cut-after", "10",
                    "--operations", "20",
                    "--workers", "1",
                    "--fsync", "off",
                    "--synchronous-commit", "off",
                    "--allow-unsafe-settings",
                ]
            )
        self.assertEqual(code, 1)
        self.assertIn("VIOLATIONS", buffer.getvalue())

    def test_unknown_driver_is_a_usage_error(self) -> None:
        buffer = io.StringIO()
        with redirect_stdout(buffer):
            code = powerloss.main(["cycle", "--driver", "teleport", "--seed", "1"])
        self.assertEqual(code, 2)


if __name__ == "__main__":
    unittest.main()
