import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import aot_evidence as ae  # noqa: E402


class AotEvidence(unittest.TestCase):
    def run_main(self, text, *flags):
        with tempfile.TemporaryDirectory() as tmp:
            log = Path(tmp) / "publish.log"
            log.write_text(text, encoding="utf-8")
            return ae.main([str(log), *flags])

    def test_aot_target_present(self):
        self.assertEqual(self.run_main("Target _WasmAotCompileApp:\n", "--expect-aot"), 0)

    def test_interpreted_log_fails_when_aot_expected(self):
        self.assertEqual(self.run_main("Target WasmBuildNative:\nEmccCompile\n", "--expect-aot"), 1)

    def test_interpreted_log_passes_without_flag(self):
        self.assertEqual(self.run_main("nothing here"), 0)

    def test_missing_log_is_bad_usage(self):
        self.assertEqual(ae.main(["/nonexistent/publish.log"]), 2)

    def test_scan_is_case_insensitive(self):
        self.assertIn("mono-aot-cross", ae.scan("MONO-AOT-CROSS"))


if __name__ == "__main__":
    unittest.main()
