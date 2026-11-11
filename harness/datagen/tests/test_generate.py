"""Determinism, stream isolation, manifest shape, verify: end-to-end."""

import hashlib
import csv
import json
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

from datagen.generate import GENERATOR_VERSION, GeneratorError, generate_dataset
from datagen.profile import load_profile

from datagen import tables


def canonical_json(obj) -> bytes:
    return (json.dumps(obj, sort_keys=True, separators=(",", ":")) + "\n").encode("utf-8")


def rows(path: Path):
    return (path.read_text(encoding="utf-8").splitlines())


_PROFILES = Path(__file__).resolve().parents[1] / "profiles"


class DeterminismTests(unittest.TestCase):
    def test_two_reruns_are_byte_identical(self):
        with tempfile.TemporaryDirectory() as tmp:
            profile = load_profile("tiny")
            first = Path(tmp) / "first"
            second = Path(tmp) / "second"
            generate_dataset(profile, 42, first)
            generate_dataset(profile, 42, second)
            names = sorted(path.name for path in first.iterdir())
            self.assertEqual(names, sorted(path.name for path in second.iterdir()))
            for name in names:
                self.assertEqual(
                    hashlib.sha256((first / name).read_bytes()).hexdigest(),
                    hashlib.sha256((second / name).read_bytes()).hexdigest(),
                    name,
                )

    def test_seed_changes_the_output(self):
        with tempfile.TemporaryDirectory() as tmp:
            profile = load_profile("tiny")
            first = Path(tmp) / "first"
            second = Path(tmp) / "second"
            generate_dataset(profile, 42, first)
            generate_dataset(profile, 43, second)
            self.assertNotEqual(
                (first / "sales_invoice.jsonl").read_bytes(),
                (second / "sales_invoice.jsonl").read_bytes(),
            )

    def test_csv_mode_is_byte_identical_across_reruns(self):
        with tempfile.TemporaryDirectory() as tmp:
            profile = load_profile("tiny")
            first = Path(tmp) / "first"
            second = Path(tmp) / "second"
            generate_dataset(profile, 42, first, mode="csv")
            generate_dataset(profile, 42, second, mode="csv")
            names = sorted(path.name for path in first.iterdir())
            self.assertEqual(names, sorted(path.name for path in second.iterdir()))
            for name in names:
                self.assertEqual(
                    (first / name).read_bytes(), (second / name).read_bytes(), name
                )


class LfEndingsTests(unittest.TestCase):
    """Every written byte keeps LF endings, so the manifest digests match on
    any platform: on Windows a text-mode open would translate ``\\n`` into
    CRLF while the verifier hashes LF bytes and reports digest violations."""

    def test_jsonl_files_contain_no_carriage_returns(self):
        with tempfile.TemporaryDirectory() as tmp:
            profile = load_profile("tiny")
            out = Path(tmp) / "out"
            generate_dataset(profile, 42, out)
            for path in sorted(out.iterdir()):
                self.assertNotIn(b"\r", path.read_bytes(), path.name)

    def test_csv_files_contain_no_carriage_returns(self):
        with tempfile.TemporaryDirectory() as tmp:
            profile = load_profile("tiny")
            out = Path(tmp) / "out"
            generate_dataset(profile, 42, out, mode="csv")
            for path in sorted(out.iterdir()):
                self.assertNotIn(b"\r", path.read_bytes(), path.name)


class RefusalTests(unittest.TestCase):
    def test_refuses_to_overwrite(self):
        with tempfile.TemporaryDirectory() as tmp:
            profile = load_profile("tiny")
            out = Path(tmp) / "out"
            generate_dataset(profile, 42, out)
            with self.assertRaises(GeneratorError):
                generate_dataset(profile, 42, out)

    def test_refuses_a_directory_that_is_not_empty(self):
        with tempfile.TemporaryDirectory() as tmp:
            profile = load_profile("tiny")
            out = Path(tmp) / "occupied"
            out.mkdir()
            (out / "keep.txt").write_bytes(b"x")
            with self.assertRaises(GeneratorError):
                generate_dataset(profile, 42, out)

    def test_empty_existing_directory_is_accepted(self):
        with tempfile.TemporaryDirectory() as tmp:
            profile = load_profile("tiny")
            out = Path(tmp) / "exists"
            out.mkdir()
            generate_dataset(profile, 42, out)
            self.assertTrue((out / "manifest.json").exists())


class ManifestTests(unittest.TestCase):
    def test_manifest_records_provenance(self):
        with tempfile.TemporaryDirectory() as tmp:
            profile = load_profile("tiny")
            out = Path(tmp) / "out"
            generate_dataset(profile, 42, out)
            manifest = json.loads((out / "manifest.json").read_text(encoding="utf-8"))
            self.assertEqual(manifest["generator"], "spot-datagen")
            self.assertEqual(manifest["generator_version"], GENERATOR_VERSION)
            self.assertEqual(manifest["profile"], "tiny")
            self.assertEqual(manifest["seed"], 42)
            self.assertEqual(manifest["format"], "jsonl")
            self.assertEqual(
                manifest["profile_sha256"],
                hashlib.sha256((_PROFILES / "tiny.toml").read_bytes()).hexdigest(),
            )
            self.assertEqual(sorted(manifest["files"]), sorted(p.name for p in out.iterdir() if p.name != "manifest.json"))
            for entry in manifest["files"].values():
                self.assertEqual(sorted(entry), ["rows", "sha256"])


class StreamIsolationTests(unittest.TestCase):
    def test_master_data_does_not_move_when_invoice_counts_change(self):
        with tempfile.TemporaryDirectory() as tmp:
            profile = load_profile("tiny")
            variant_text = (_PROFILES / "tiny.toml").read_text(encoding="utf-8")
            variant_text = variant_text.replace("sales_invoices = 60", "sales_invoices = 80")
            variant_text = variant_text.replace("payments = 27", "payments = 33")
            variant_path = Path(tmp) / "tiny-more.toml"
            variant_path.write_text(variant_text, encoding="utf-8")
            variant = load_profile(str(variant_path))

            base_out = Path(tmp) / "base"
            more_out = Path(tmp) / "more"
            generate_dataset(profile, 42, base_out)
            generate_dataset(variant, 42, more_out)

            for master in ("companies.jsonl", "warehouses.jsonl", "items.jsonl", "customers.jsonl", "suppliers.jsonl"):
                self.assertEqual(
                    (base_out / master).read_bytes(),
                    (more_out / master).read_bytes(),
                    master,
                )
            self.assertEqual(
                len(rows(more_out / "sales_invoice.jsonl")), 80
            )
            self.assertEqual(
                len(rows(more_out / "payment.jsonl")), 33
            )

    def test_journal_counts_do_not_move_master_or_invoices(self):
        with tempfile.TemporaryDirectory() as tmp:
            variant_text = (_PROFILES / "tiny.toml").read_text(encoding="utf-8")
            variant_text = variant_text.replace("journal_entries = 20", "journal_entries = 24", 1)
            variant_path = Path(tmp) / "tiny-more-journals.toml"
            variant_path.write_text(variant_text, encoding="utf-8")
            variant = load_profile(str(variant_path))

            base_out = Path(tmp) / "base"
            more_out = Path(tmp) / "more"
            generate_dataset(load_profile("tiny"), 42, base_out)
            generate_dataset(variant, 42, more_out)

            self.assertEqual(
                (base_out / "items.jsonl").read_bytes(),
                (more_out / "items.jsonl").read_bytes(),
            )
            self.assertEqual(
                (base_out / "sales_invoice.jsonl").read_bytes(),
                (more_out / "sales_invoice.jsonl").read_bytes(),
            )
            self.assertEqual(len(rows(more_out / "journal_entry.jsonl")), 24)


class CsvParityTests(unittest.TestCase):
    def test_csv_rows_match_the_jsonl_tree(self):
        with tempfile.TemporaryDirectory() as tmp:
            profile = load_profile("tiny")
            jsonl_out = Path(tmp) / "jsonl"
            csv_out = Path(tmp) / "csv"
            generate_dataset(profile, 42, jsonl_out)
            generate_dataset(profile, 42, csv_out, mode="csv")

            for table_name in tables.TABLES:
                info = tables.TABLES[table_name]
                parent_rows = [
                    json.loads(line)
                    for line in (jsonl_out / tables.file_name(table_name, "jsonl"))
                    .read_text(encoding="utf-8")
                    .splitlines()
                ]
                with open(
                    csv_out / tables.file_name(table_name, "csv"),
                    newline="",
                    encoding="utf-8",
                ) as handle:
                    csv_rows = list(csv.reader(handle))[1:]
                self.assertEqual(len(csv_rows), len(parent_rows), table_name)
                key = info["columns"][0][0]
                for parent_row, csv_row in zip(parent_rows, csv_rows):
                    self.assertEqual(parent_row[key], csv_row[0], table_name)

                child = info.get("child")
                if child is not None:
                    local_lines = info["lines_field"]
                    with open(
                        csv_out / tables.file_name(child["name"], "csv"),
                        newline="",
                        encoding="utf-8",
                    ) as handle:
                        child_rows = list(csv.reader(handle))[1:]
                    inline = sum(len(row.get(local_lines, [])) for row in parent_rows)
                    self.assertEqual(len(child_rows), inline, child["name"])
                    position = 0
                    for row in parent_rows:
                        inline_lines = row.get(local_lines, [])
                        segment = child_rows[position : position + len(inline_lines)]
                        self.assertTrue(
                            all(line[0] == row[key] for line in segment),
                            f"{child['name']}: rows not grouped under their parent",
                        )
                        position += len(inline_lines)


if __name__ == "__main__":
    unittest.main()