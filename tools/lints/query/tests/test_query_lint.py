from __future__ import annotations

import contextlib
import io
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import query_lint  # noqa: E402


class QueryLintTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory()
        self.root = Path(self.temporary.name)
        self.crates = self.root / "crates"
        self.allow = self.root / "allow.toml"
        self.allow.write_text("allow = []\n", encoding="utf-8")

    def tearDown(self) -> None:
        self.temporary.cleanup()

    def write(self, relative: str, source: str) -> None:
        path = self.root / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(source, encoding="utf-8")

    def run_lint(self) -> tuple[int, str]:
        output = io.StringIO()
        with contextlib.redirect_stdout(output), contextlib.redirect_stderr(output):
            result = query_lint.run(self.root, self.allow)
        return result, output.getvalue()

    def test_each_sql_statement_shape_is_rejected(self) -> None:
        examples = [
            '"SELECT id FROM records"',
            '"INSERT INTO records VALUES (1)"',
            '"UPDATE records SET value = 1"',
            '"DELETE FROM records"',
            '"CREATE TABLE records (id INT)"',
            '"ALTER TABLE records ADD COLUMN value INT"',
            '"DROP TABLE records"',
            '"WITH recent AS (SELECT 1) SELECT * FROM recent"',
            '"LISTEN changes"',
            '"NOTIFY changes"',
        ]
        for index, statement in enumerate(examples):
            with self.subTest(statement=statement):
                self.write(f"crates/apps/a/src/{index}.rs", f"let q = {statement};\n")
                result, output = self.run_lint()
                self.assertEqual(result, 1)
                self.assertIn("G5: crates/apps/a/src/", output)
                self.assertIn(":1: SQL ", output)
                (self.crates / "apps/a/src" / f"{index}.rs").unlink()

    def test_each_database_driver_path_is_rejected(self) -> None:
        drivers = (
            "sqlx", "tokio_postgres", "postgres", "diesel", "sea_orm",
            "deadpool_postgres", "rusqlite",
        )
        for index, driver in enumerate(drivers):
            with self.subTest(driver=driver):
                self.write(f"crates/apps/a/src/{index}.rs", f"use {driver}::Client;\n")
                result, output = self.run_lint()
                self.assertEqual(result, 1)
                self.assertIn(f"database driver crate path: {driver}", output)
                (self.crates / "apps/a/src" / f"{index}.rs").unlink()

    def test_leading_absolute_driver_path_is_rejected(self) -> None:
        self.write("crates/apps/a/src/lib.rs", "let client = ::postgres::Client;\n")
        result, output = self.run_lint()
        self.assertEqual(result, 1)
        self.assertIn("database driver crate path: postgres", output)

    def test_non_driver_identifiers_and_qualified_paths_are_ignored(self) -> None:
        self.write(
            "crates/apps/a/src/lib.rs",
            "use spot_storage::backend::postgres::Pool;\n"
            "cfg.postgres;\n"
            "Backend::postgres;\n"
            "let postgres = 1;\n"
            "f(postgres, x);\n",
        )
        result, output = self.run_lint()
        self.assertEqual(result, 0, output)

    def test_char_literals_and_lifetimes_do_not_corrupt_tokenization(self) -> None:
        char_literals = ("'\"'", "'\\''", "'\\\\'", "'\\u{27}'", "b'\"'", "b'\\''", "b'\\\\'")
        source = " ".join(char_literals) + " fn lifetime<'a>(value: &'static str) {}"
        code, literals = query_lint.tokenize(source)
        self.assertEqual(literals, [])
        for char_literal in char_literals:
            self.assertNotIn(char_literal, code)
        self.assertIn("'a", code)
        self.assertIn("'static", code)

        self.write(
            "crates/apps/a/src/char.rs",
            "fn f() -> char { let q = '\"'; q }\n"
            "let apostrophe = '\\'';\n"
            "let slash = '\\\\';\n"
            "let unicode = '\\u{27}';\n"
            "let byte_quote = b'\"';\n"
            "let byte_apostrophe = b'\\'';\n"
            "let byte_slash = b'\\\\';\n"
            "// SELECT id FROM the comment\n"
            "fn lifetime<'a>(value: &'a str) { let q = \"SELECT id FROM table\"; }\n"
            "'outer: loop { break 'outer; }\n",
        )
        result, output = self.run_lint()
        self.assertEqual(result, 1)
        self.assertEqual(output, "G5: crates/apps/a/src/char.rs:9: SQL SELECT … FROM statement\n")

    def test_comments_and_doc_comments_are_ignored_including_nested_blocks(self) -> None:
        self.write(
            "crates/apps/a/src/lib.rs",
            "/// SELECT id FROM docs\n"
            "// use sqlx::Client;\n"
            "/* DELETE FROM t /* nested NOTIFY events */ still comment */\n"
            "fn good() {}\n",
        )
        result, output = self.run_lint()
        self.assertEqual(result, 0, output)

    def test_backend_and_storage_crates_are_excluded(self) -> None:
        self.write("crates/backends/postgres/src/lib.rs", 'use sqlx::Client;\nlet q = "SELECT x FROM y";')
        self.write("crates/framework/spot-storage/src/lib.rs", 'use postgres::Client;\n')
        result, output = self.run_lint()
        self.assertEqual(result, 0, output)

    def test_allowlist_requires_reason_and_matches_path_and_finding(self) -> None:
        self.write("crates/apps/a/src/legacy.rs", 'let q = "SELECT id FROM legacy";')
        self.allow.write_text(
            '[[allow]]\npath = "crates/apps/a/src/legacy.rs"\n'
            'pattern = "SQL SELECT"\nreason = "Temporary bridge"\n',
            encoding="utf-8",
        )
        result, output = self.run_lint()
        self.assertEqual(result, 0, output)

        self.allow.write_text(
            '[[allow]]\npath = "crates/apps/a/src/legacy.rs"\npattern = "SQL SELECT"\n',
            encoding="utf-8",
        )
        result, output = self.run_lint()
        self.assertEqual(result, 2)
        self.assertIn("requires path, pattern and reason", output)

    def test_raw_strings_escaped_quotes_and_plain_english(self) -> None:
        self.write(
            "crates/apps/a/src/lib.rs",
            r'''let ordinary = "say \"SELECT id FROM hidden\"";
let raw = r###"DELETE FROM records"###;
let prose = "select a customer when searching";
''',
        )
        result, output = self.run_lint()
        self.assertEqual(result, 1, output)
        self.assertIn("SQL DELETE FROM statement", output)
        self.assertIn("SQL SELECT … FROM statement", output)

    def test_missing_crates_directory_is_successful(self) -> None:
        result, output = self.run_lint()
        self.assertEqual(result, 0)
        self.assertIn("no crates/ directory", output)


if __name__ == "__main__":
    unittest.main()
