"""CLI exit codes: 0 on success, 1 with stderr on failure, argparse for misuse."""

import contextlib
import io
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

from datagen.generate import generate_dataset
from datagen.profile import load_profile
from datagen import cli


def run_cli(*argv):
    out, err = io.StringIO(), io.StringIO()
    with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
        code = cli.main(list(argv))
    return code, out.getvalue(), err.getvalue()


class ListProfilesTests(unittest.TestCase):
    def test_lists_the_shipped_profiles(self):
        code, out, _err = run_cli("--list-profiles")
        self.assertEqual(code, 0)
        names = out.splitlines()
        self.assertEqual(len(names), 4)
        self.assertEqual(set(names), {"tiny", "small-trader", "mid-distributor", "large"})

    def test_list_profile_cannot_be_combined_with_verify(self):
        with self.assertRaises(SystemExit):
            run_cli("--list-profiles", "--verify", ".")


class VerifyModeTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls._tmp = tempfile.TemporaryDirectory()
        cls.out = Path(cls._tmp.name) / "out"
        generate_dataset(load_profile("tiny"), 42, cls.out)
        cls.out_csv = Path(cls._tmp.name) / "out-csv"
        generate_dataset(load_profile("tiny"), 42, cls.out_csv, mode="csv")

    @classmethod
    def tearDownClass(cls):
        cls._tmp.cleanup()

    def test_jsonl_output_is_ok(self):
        code, out, err = run_cli("--verify", str(self.out))
        self.assertEqual(code, 0, err)
        self.assertIn("verify: OK", out)

    def test_csv_output_is_ok(self):
        code, out, err = run_cli("--verify", str(self.out_csv))
        self.assertEqual(code, 0, err)
        self.assertIn("verify: OK", out)

    def test_missing_directory_exits_1(self):
        code, _out, err = run_cli("--verify", str(Path(self._tmp.name) / "absent"))
        self.assertEqual(code, 1)
        self.assertIn("verify: FAILED", err)

    def test_corrupted_directory_exits_1(self):
        bad = Path(self._tmp.name) / "bad"
        bad.mkdir(exist_ok=True)
        (bad / "companies.jsonl").write_bytes(b"{}\n")
        code, _out, err = run_cli("--verify", str(bad))
        self.assertEqual(code, 1)
        self.assertIn("verify: FAILED", err)


class GenerateModeTests(unittest.TestCase):
    def test_generates_and_reports_row_counts(self):
        with tempfile.TemporaryDirectory() as tmp:
            out = Path(tmp) / "out"
            code, out_text, err = run_cli("--profile", "tiny", "--seed", "42", "--out", str(out))
            self.assertEqual(code, 0, err)
            self.assertIn("sales_invoice.jsonl: 60 rows", out_text)
            self.assertIn("payment.jsonl: 27 rows", out_text)
            self.assertIn("manifest:", out_text)
            self.assertIn("written:", out_text)

    def test_csv_mode_reports_csv_file_names(self):
        with tempfile.TemporaryDirectory() as tmp:
            out = Path(tmp) / "out"
            code, out_text, err = run_cli(
                "--profile", "tiny", "--seed", "42", "--format", "csv", "--out", str(out)
            )
            self.assertEqual(code, 0, err)
            self.assertIn("sales_invoice.csv: 60 rows", out_text)
            self.assertIn("sales_invoice_line.csv:", out_text)
            self.assertTrue((out / "sales_invoice.csv").exists())

    def test_refusing_to_overwrite_exits_1(self):
        with tempfile.TemporaryDirectory() as tmp:
            out = Path(tmp) / "out"
            code, _out, _err = run_cli("--profile", "tiny", "--seed", "42", "--out", str(out))
            self.assertEqual(code, 0)
            code, _out, err = run_cli("--profile", "tiny", "--seed", "42", "--out", str(out))
            self.assertEqual(code, 1)
            self.assertIn("generate: FAILED", err)

    def test_unknown_profile_exits_1(self):
        with tempfile.TemporaryDirectory() as tmp:
            code, _out, err = run_cli(
                "--profile", "no-such-profile", "--seed", "1", "--out", str(Path(tmp) / "out")
            )
            self.assertEqual(code, 1)
            self.assertIn("generate: FAILED", err)

    def test_seed_must_be_an_integer(self):
        with self.assertRaises(SystemExit):
            run_cli("--profile", "tiny", "--seed", "not-a-number", "--out", "x")

    def test_missing_out_is_an_argparse_error(self):
        with self.assertRaises(SystemExit):
            run_cli("--profile", "tiny", "--seed", "1")


if __name__ == "__main__":
    unittest.main()
