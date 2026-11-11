"""Schema/load.sql coherence tests for the provisional SQL staging file.

The CSV files in every dataset directory carry a header row with the exact
column names of the table schema in ``datagen/tables.py``. The DDL in
``../sql/schema.sql`` and the load order in ``../sql/load.sql`` must agree
with those tables, or nothing loaded from a dataset is trustworthy.
"""

import re
import unittest
from pathlib import Path

from datagen import tables

SCHEMA_PATH = Path(__file__).resolve().parents[1] / "sql" / "schema.sql"
LOAD_PATH = Path(__file__).resolve().parents[1] / "sql" / "load.sql"

COPY_RE = re.compile(r"^\\copy\s+(\w+)\b", re.MULTILINE)
TABLE_RE = re.compile(r"CREATE TABLE (\w+)\s*\((.*?)\);", re.DOTALL)
CONSTRAINT_LINES = {"CONSTRAINT", "PRIMARY", "FOREIGN", "UNIQUE", "CHECK"}

# Fine-grained column classes by name. The schema keeps decimal columns
# exactly as wide as the fixed-scale strings the generator writes.
MONEY_COLUMNS = {
    "standard_rate", "valuation_rate", "rate", "amount",
    "total", "debit", "credit",
}
MONEY_TYPE = "numeric(12,2)"
QUANTITY_COLUMNS = {"quantity"}
QUANTITY_TYPE = "numeric(14,3)"


def parse_columns(sql: str) -> dict:
    """Map each CREATE TABLE block to its column names, in order."""
    parsed: dict[str, list[str]] = {}
    for match in TABLE_RE.finditer(sql):
        columns: list[str] = []
        for raw in match.group(2).splitlines():
            line = raw.split("--", 1)[0].strip().rstrip(",")
            if not line:
                continue
            first = line.split(None, 1)[0].upper()
            if first in CONSTRAINT_LINES:
                continue
            columns.append(line.split(None, 1)[0])
        parsed[match.group(1)] = columns
    return parsed


def column_types(sql: str) -> dict:
    """Map each CREATE TABLE block to {column name: declared type}."""
    result: dict[str, dict[str, str]] = {}
    for match in TABLE_RE.finditer(sql):
        types: dict[str, str] = {}
        for raw in match.group(2).splitlines():
            line = raw.split("--", 1)[0].strip().rstrip(",")
            if not line:
                continue
            first = line.split(None, 1)[0].upper()
            if first in CONSTRAINT_LINES:
                continue
            parts = line.split()
            types[parts[0]] = parts[1]
        result[match.group(1)] = types
    return result


def parent_column_pairs(table_name: str) -> list:
    """(column, kind) pairs of a parent table's CSV file."""
    return [(column, kind) for column, kind in tables.TABLES[table_name]["columns"]]


def child_column_pairs(parent_name: str) -> list:
    """(column, kind) pairs of one child CSV file."""
    return [(column, kind) for column, kind in tables.full_child_columns(parent_name)]


def expected_columns(table_name: str) -> list:
    return [column for column, _ in parent_column_pairs(table_name)]


def child_file_names() -> dict:
    """Map each child CSV table name to its parent table name."""
    return {
        info["child"]["name"]: parent
        for parent, info in tables.TABLES.items()
        if info["child"]
    }


def declared_type(column: str, kind: str) -> str:
    if column in QUANTITY_COLUMNS:
        return QUANTITY_TYPE
    if column in MONEY_COLUMNS:
        return MONEY_TYPE
    if column == "date":
        return "date"
    if kind == "int":
        return "integer"
    if kind == "bool":
        return "boolean"
    return "text"


class SchemaCoherence(unittest.TestCase):
    """schema.sql blocks must mirror the datagen table schema."""

    def setUp(self):
        self.ddl = SCHEMA_PATH.read_text()
        self.parsed = parse_columns(self.ddl)
        self.types = column_types(self.ddl)

    def test_every_table_has_a_create(self):
        for name in tables.TABLES:
            self.assertIn(name, self.parsed, f"schema.sql is missing CREATE TABLE {name!r}")

    def test_table_columns_match_csv_headers(self):
        for name in tables.TABLES:
            self.assertEqual(
                self.parsed[name],
                expected_columns(name),
                f"columns of {name!r} in schema.sql do not match the CSV header",
            )
        for child, parent in child_file_names().items():
            self.assertEqual(
                self.parsed[child],
                [column for column, _ in child_column_pairs(parent)],
                f"columns of {child!r} in schema.sql do not match the CSV header",
            )

    def test_column_types(self):
        for name in tables.TABLES:
            types = self.types[name]
            for column, kind in parent_column_pairs(name):
                with self.subTest(table=name, column=column):
                    self.assertEqual(types[column], declared_type(column, kind))
        for child, parent in child_file_names().items():
            types = self.types[child]
            for column, kind in child_column_pairs(parent):
                with self.subTest(table=child, column=column):
                    self.assertEqual(types[column], declared_type(column, kind))

    def ddl_block(self, name: str) -> str:
        """Return the CREATE TABLE block text for one table."""
        for match in TABLE_RE.finditer(self.ddl):
            if match.group(1) == name:
                return match.group(0)
        self.fail(f"missing CREATE TABLE {name!r}")
        raise AssertionError("unreachable")

    def test_primary_keys(self):
        for name, info in tables.TABLES.items():
            key = info["columns"][0][0]
            block = self.ddl_block(name)
            self.assertIn(f"PRIMARY KEY ({key})", block, f"{name!r} lacks its primary key")
        for child, parent in child_file_names().items():
            key = f"{tables.child_key(parent)}, line"
            block = self.ddl_block(child)
            self.assertIn(f"PRIMARY KEY ({key})", block, f"{child!r} lacks its primary key")

    def test_foreign_key_targets_exist(self):
        for match in re.finditer(r"REFERENCES (\w+)", self.ddl):
            self.assertIn(
                match.group(1),
                self.parsed,
                f"foreign key references unknown table {match.group(1)!r}",
            )

    def test_child_lines_reference_their_parent(self):
        parent_pk = {
            name: info["columns"][0][0] for name, info in tables.TABLES.items()
        }
        for child, parent in child_file_names().items():
            block = self.ddl_block(child)
            match = re.search(rf"REFERENCES {parent} \((\w+)\)", block)
            self.assertIsNotNone(
                match,
                f"{child!r} does not reference its parent {parent!r} in schema.sql",
            )
            self.assertEqual(
                match.group(1),
                parent_pk[parent],
                f"{child!r} references {parent!r} by a column that is not its key",
            )
            self.assertEqual(
                self.parsed[child][0],
                tables.child_key(parent),
                f"{child!r}'s first column is not its parent key",
            )


class LoadOrder(unittest.TestCase):
    """load.sql must copy every CSV file, in dependency order."""

    def setUp(self):
        self.sql = LOAD_PATH.read_text()
        self.copied = COPY_RE.findall(self.sql)

    def test_every_csv_table_is_copied(self):
        self.assertEqual(set(self.copied), set(tables.TABLES) | set(child_file_names()))

    def test_copy_sources_are_bare_file_names(self):
        for name in set(tables.TABLES) | set(child_file_names()):
            self.assertIn(f"FROM '{name}.csv'", self.sql, f"load.sql does not load {name}.csv")

    def test_parents_load_before_their_lines(self):
        positions = {name: i for i, name in enumerate(self.copied)}
        for child, parent in child_file_names().items():
            self.assertLess(
                positions[parent],
                positions[child],
                f"load.sql copies {child!r} before its parent {parent!r}",
            )

    def test_master_data_loads_before_transactions(self):
        positions = {name: i for i, name in enumerate(self.copied)}
        self.assertLess(positions["items"], positions["sales_invoice"])
        self.assertLess(positions["warehouses"], positions["sales_invoice"])


if __name__ == "__main__":
    unittest.main()