from __future__ import annotations

import contextlib
import io
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import table_prefix_lint  # noqa: E402

REPO_ROOT = Path(__file__).resolve().parents[4]

# The closed set of crates that were allowed when the gate shipped. The allow-list
# may shrink (each rename task deletes its file) but must never gain a crate.
KNOWN_LEGACY_CRATES = frozenset(
    {
        "spot-audit-store",
        "spot-bpm-pg",
        "spot-dag-pg",
        "spot-installer-pg",
        "spot-link-exchange-pg",
        "spot-quarantine-pg",
        "spot-sso-pg",
        "spot-tenant-link-pg",
    }
)

# Each statement shape, unprefixed (a finding) and prefixed (accepted).
UNPREFIXED = {
    "table": "CREATE TABLE legacy (id integer)",
    "unlogged table": "CREATE UNLOGGED TABLE legacy (id integer)",
    "table if not exists": "CREATE TABLE IF NOT EXISTS legacy (id integer)",
    "index": "CREATE INDEX legacy_idx ON spot_thing (id)",
    "unique index": "CREATE UNIQUE INDEX legacy_idx ON spot_thing (id)",
    "concurrent index": "CREATE INDEX CONCURRENTLY IF NOT EXISTS legacy_idx ON spot_thing (id)",
    "index if not exists": "CREATE INDEX IF NOT EXISTS legacy_idx ON spot_thing (id)",
    "sequence": "CREATE SEQUENCE legacy_seq",
    "sequence if not exists": "CREATE SEQUENCE IF NOT EXISTS legacy_seq",
    "view": "CREATE VIEW legacy_view AS SELECT 1",
    "materialized view": "CREATE MATERIALIZED VIEW legacy_mv AS SELECT 1",
    "type": "CREATE TYPE legacy_type AS ENUM ('a')",
    "constraint": "ALTER TABLE spot_thing ADD CONSTRAINT legacy_ck CHECK (id > 0)",
    "references": "CREATE TABLE spot_child (id uuid NOT NULL REFERENCES legacy (id))",
    "rename": "ALTER TABLE spot_thing RENAME TO legacy",
}
PREFIXED = {kind: statement.replace("legacy", "spot_legacy") for kind, statement in UNPREFIXED.items()}


class TempTreeTestCase(unittest.TestCase):
    """A scratch tree with crates/backends and an allow/ directory."""

    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory()
        self.root = Path(self.temporary.name)
        (self.root / "crates" / "backends").mkdir(parents=True)
        self.allow = self.root / "allow"
        self.allow.mkdir()

    def tearDown(self) -> None:
        self.temporary.cleanup()

    def write(self, relative: str, source: str) -> Path:
        path = self.root / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(source, encoding="utf-8")
        return path

    def rust(self, statement: str, crate: str = "spot-demo-pg", name: str = "schema.rs") -> Path:
        return self.write(f"crates/backends/{crate}/src/{name}", f'pub const DDL: &str = "{statement}";\n')

    def allow_entry(
        self,
        crate: str,
        *,
        reason: str | None = "Creates unprefixed tables while the rename task is open.",
        rename_task: str | None = "WP-X.1",
        file_name: str | None = None,
    ) -> Path:
        lines = [f'crate = "{crate}"']
        if reason is not None:
            lines.append(f'reason = "{reason}"')
        if rename_task is not None:
            lines.append(f'rename_task = "{rename_task}"')
        return self.write(f"allow/{file_name or crate}.toml", "\n".join(lines) + "\n")

    def run_lint(self) -> tuple[int, str]:
        output = io.StringIO()
        with contextlib.redirect_stdout(output), contextlib.redirect_stderr(output):
            code = table_prefix_lint.run(self.root, self.allow, output=output)
        return code, output.getvalue()

    def findings(self) -> list[table_prefix_lint.Finding]:
        scan, _ = table_prefix_lint.scan_tree(self.root)
        return scan.findings


class StatementKindTests(TempTreeTestCase):
    def test_each_statement_kind_is_flagged_when_unprefixed(self) -> None:
        for index, (kind, statement) in enumerate(UNPREFIXED.items()):
            with self.subTest(kind=kind):
                path = self.write(f"crates/backends/spot-demo-pg/src/{index}.rs", f'let q = "{statement}";\n')
                code, output = self.run_lint()
                self.assertEqual(code, 1, output)
                self.assertIn("does not start with `spot_`", output)
                self.assertIn(f"crates/backends/spot-demo-pg/src/{index}.rs:1:", output)
                self.assertIn("legacy", output)
                path.unlink()

    def test_each_statement_kind_is_accepted_when_prefixed(self) -> None:
        for kind, statement in PREFIXED.items():
            with self.subTest(kind=kind):
                self.write(f"crates/backends/spot-demo-pg/src/{kind.replace(' ', '_')}.rs", f'let q = "{statement}";\n')
        code, output = self.run_lint()
        self.assertEqual(code, 0, output)
        self.assertIn("clean:", output)
        self.assertNotIn("does not start with", output)

    def test_the_kind_label_names_what_was_created(self) -> None:
        self.rust("CREATE TABLE legacy (id integer)")
        self.rust("CREATE SEQUENCE legacy_seq", name="seq.rs")
        self.rust("CREATE TYPE legacy_type AS ENUM ('a')", name="ty.rs")
        scan = table_prefix_lint.scan_tree(self.root)[0]
        self.assertEqual(
            [(finding.kind, finding.name) for finding in scan.findings],
            [("table", "legacy"), ("sequence", "legacy_seq"), ("type", "legacy_type")],
        )

    def test_rename_reports_the_new_name_only(self) -> None:
        self.rust("ALTER TABLE spot_thing RENAME TO legacy")
        self.assertEqual([(f.kind, f.name) for f in self.findings()], [("renamed table", "legacy")])

    def test_rename_to_a_prefixed_name_is_accepted(self) -> None:
        self.rust("ALTER TABLE legacy RENAME TO spot_legacy")
        self.assertEqual(self.run_lint()[0], 0)


class ScopeTests(TempTreeTestCase):
    def test_schema_qualified_name_is_flagged_unless_the_schema_is_the_control_plane(self) -> None:
        self.rust("CREATE TABLE bi.v1_party (id uuid)")
        self.rust("CREATE TABLE spot_control.tenant (id uuid)", name="control.rs")
        scan = table_prefix_lint.scan_tree(self.root)[0]
        self.assertEqual([(f.kind, f.name) for f in scan.findings], [("table", "v1_party")])
        self.assertEqual(scan.exempt, 1)

    def test_every_control_schema_object_and_its_indexes_are_exempt(self) -> None:
        self.rust(
            "CREATE SCHEMA IF NOT EXISTS spot_control; "
            "CREATE TABLE spot_control.link_registry (id uuid); "
            "CREATE INDEX link_registry_a ON spot_control.link_registry (id); "
            "CREATE TABLE spot_migration_control.journal (id uuid)"
        )
        code, output = self.run_lint()
        self.assertEqual(code, 0, output)
        self.assertIn("3 in a control schema", output)

    def test_quoted_schema_and_name_are_read_without_quotes(self) -> None:
        self.write(
            "crates/backends/spot-demo-pg/src/quoted.rs",
            'let a = "CREATE TABLE \\"bi\\".\\"v1_party\\" (id uuid)";\n'
            'let b = "CREATE TABLE \\"spot_control\\".\\"tenant\\" (id uuid)";\n'
            'let c = "CREATE TABLE \\"spot_quoted\\" (id uuid)";\n',
        )
        scan = table_prefix_lint.scan_tree(self.root)[0]
        self.assertEqual([f.name for f in scan.findings], ["v1_party"])
        self.assertEqual(scan.exempt, 1)

    def test_comments_and_doc_comments_are_ignored(self) -> None:
        self.write(
            "crates/backends/spot-demo-pg/src/notes.rs",
            "// CREATE TABLE legacy (id integer)\n"
            "/// CREATE TABLE legacy (id integer)\n"
            "//! CREATE SEQUENCE legacy_seq\n"
            "/* CREATE INDEX legacy_idx ON spot_thing (id);\n"
            "   /* nested ADD CONSTRAINT legacy_ck CHECK (true) */ still a comment */\n"
            'fn good() { let sql = "CREATE TABLE spot_fine (id integer)"; }\n',
        )
        code, output = self.run_lint()
        self.assertEqual(code, 0, output)

    def test_raw_string_literals_are_scanned(self) -> None:
        self.write(
            "crates/backends/spot-demo-pg/src/raw.rs",
            'let plain = r"CREATE TABLE legacy (id integer)";\n'
            'let hashed = r#"CREATE INDEX legacy_idx ON spot_thing (id)"#;\n'
            'let doubled = r##"CREATE SEQUENCE legacy_seq"##;\n',
        )
        scan = table_prefix_lint.scan_tree(self.root)[0]
        self.assertEqual(sorted(f.kind for f in scan.findings), ["index", "sequence", "table"])

    def test_test_directories_benches_and_examples_are_ignored(self) -> None:
        for directory in ("tests", "benches", "examples"):
            self.write(f"crates/backends/spot-demo-pg/{directory}/fixture.rs", 'let q = "CREATE TABLE legacy (id integer)";\n')
        self.assertEqual(self.run_lint()[0], 0)

    def test_cfg_test_items_are_ignored_but_production_code_after_them_is_not(self) -> None:
        self.write(
            "crates/backends/spot-demo-pg/src/ddl.rs",
            'pub const DDL: &str = "CREATE TABLE spot_ok (id integer)";\n'
            "#[cfg(test)]\nmod tests {\n"
            '    #[test]\n    fn builds_sql() { assert_eq!(ddl(), "CREATE TABLE \\"party\\" (\\"id\\" uuid, CONSTRAINT \\"party_pkey\\" PRIMARY KEY (id))"); }\n'
            "}\n",
        )
        self.assertEqual(self.run_lint()[0], 0)
        self.write(
            "crates/backends/spot-demo-pg/src/ddl.rs",
            'pub const DDL: &str = "CREATE TABLE spot_ok (id integer)";\n'
            "#[cfg(test)]\nmod tests {\n"
            '    fn builds_sql() { let _ = "CREATE TABLE party (id uuid)"; }\n'
            "}\n"
            'pub const LATE: &str = "ALTER TABLE spot_ok RENAME TO party";\n',
        )
        scan = table_prefix_lint.scan_tree(self.root)[0]
        self.assertEqual([(f.kind, f.name) for f in scan.findings], [("renamed table", "party")])

    def test_temp_tables_are_exempt(self) -> None:
        self.rust("CREATE TEMP TABLE scratch (id integer); CREATE TEMPORARY TABLE scratch2 (id integer)")
        code, output = self.run_lint()
        self.assertEqual(code, 0, output)

    def test_drop_constraint_and_prose_are_not_creations(self) -> None:
        self.rust("ALTER TABLE spot_thing DROP CONSTRAINT IF EXISTS legacy_ck")
        self.rust(
            'the foreign key reports a constraint violation and references the other row',
            name="prose.rs",
        )
        self.assertEqual(self.run_lint()[0], 0)

    def test_a_name_built_from_a_prefixed_format_string_is_read_as_prefixed(self) -> None:
        self.rust("CREATE TABLE spot_{entity} (id integer)")
        self.assertEqual(self.run_lint()[0], 0)


class PlaceholderTests(TempTreeTestCase):
    def test_a_placeholder_name_is_counted_unchecked_and_does_not_fail(self) -> None:
        self.rust("CREATE TABLE {TABLE} (id integer)")
        self.rust("CREATE INDEX {name} ON spot_thing (id)", name="index.rs")
        self.rust('CREATE TABLE IF NOT EXISTS \\"{TABLE}\\" (id integer)', name="escaped.rs")
        code, output = self.run_lint()
        self.assertEqual(code, 0, output)
        self.assertIn("unchecked: 3 object name(s)", output)
        self.assertIn("crates/backends/spot-demo-pg/src/schema.rs:1: table name `{TABLE}`", output)
        self.assertIn("index name `{name}`", output)

    def test_counts_add_up_to_every_object_read(self) -> None:
        self.rust("CREATE TABLE spot_ok (id integer)")
        self.rust("CREATE TABLE legacy (id integer)", name="legacy.rs")
        self.rust("CREATE TABLE {TABLE} (id integer)", name="placeholder.rs")
        self.rust("CREATE TABLE spot_control.tenant (id uuid)", name="control.rs")
        scan = table_prefix_lint.scan_tree(self.root)[0]
        self.assertEqual(scan.checked, 4)
        self.assertEqual(scan.checked, scan.prefixed + scan.exempt + len(scan.unchecked) + len(scan.findings))
        self.assertEqual((scan.prefixed, scan.exempt, len(scan.findings)), (1, 1, 1))


class LineNumberTests(TempTreeTestCase):
    def test_a_multiline_literal_reports_the_line_of_the_name(self) -> None:
        self.write(
            "crates/backends/spot-demo-pg/src/multiline.rs",
            "pub const DDL: &str = \"\\\n"
            "CREATE TABLE spot_first (id integer); \\\n"
            "CREATE TABLE legacy_second (id integer);\";\n",
        )
        scan = table_prefix_lint.scan_tree(self.root)[0]
        self.assertEqual([(f.name, f.line) for f in scan.findings], [("legacy_second", 3)])

    def test_a_name_after_a_line_continuation_keeps_the_source_line(self) -> None:
        self.write(
            "crates/backends/spot-demo-pg/src/continued.rs",
            'pub const DDL: &str = "CREATE TABLE spot_first (id integer);\\n\\\n'
            'CREATE INDEX legacy_idx ON spot_first (id)";\n',
        )
        scan = table_prefix_lint.scan_tree(self.root)[0]
        self.assertEqual([(f.name, f.line) for f in scan.findings], [("legacy_idx", 2)])


class AllowListTests(TempTreeTestCase):
    def test_an_allowed_crate_passes_with_its_count_printed(self) -> None:
        self.rust("CREATE TABLE legacy (id integer)")
        self.rust("CREATE INDEX legacy_idx ON legacy (id)", name="index.rs")
        self.allow_entry("spot-demo-pg")
        code, output = self.run_lint()
        self.assertEqual(code, 0, output)
        self.assertIn("allowed (rename pending): 2 finding(s) in 1 crate(s): spot-demo-pg (2)", output)

    def test_a_finding_in_an_unlisted_crate_fails(self) -> None:
        self.rust("CREATE TABLE legacy (id integer)", crate="spot-other-pg")
        self.rust("CREATE TABLE legacy (id integer)")
        self.allow_entry("spot-demo-pg")
        code, output = self.run_lint()
        self.assertEqual(code, 1, output)
        self.assertIn("crates/backends/spot-other-pg/src/schema.rs:1: table `legacy`", output)
        self.assertIn("1 unprefixed object(s) in 1 crate(s)", output)

    def test_a_stale_allow_entry_fails(self) -> None:
        self.rust("CREATE TABLE spot_renamed (id integer)")
        self.allow_entry("spot-demo-pg")
        code, output = self.run_lint()
        self.assertEqual(code, 1, output)
        self.assertIn("stale entry, crate 'spot-demo-pg' creates no unprefixed object", output)

    def test_an_allow_entry_without_a_rename_task_fails(self) -> None:
        self.rust("CREATE TABLE legacy (id integer)")
        self.allow_entry("spot-demo-pg", rename_task=None)
        code, output = self.run_lint()
        self.assertEqual(code, 1, output)
        self.assertIn("missing or empty rename_task", output)

    def test_an_allow_entry_without_a_reason_fails(self) -> None:
        self.rust("CREATE TABLE legacy (id integer)")
        self.allow_entry("spot-demo-pg", reason=None)
        code, output = self.run_lint()
        self.assertEqual(code, 1, output)
        self.assertIn("missing or empty reason", output)

    def test_an_allow_entry_for_a_missing_crate_directory_fails(self) -> None:
        self.allow_entry("spot-absent-pg")
        code, output = self.run_lint()
        self.assertEqual(code, 1, output)
        self.assertIn("crate 'spot-absent-pg' has no directory under crates/backends/", output)

    def test_an_allow_file_named_after_another_crate_fails(self) -> None:
        self.rust("CREATE TABLE legacy (id integer)")
        self.allow_entry("spot-demo-pg", file_name="spot-renamed-pg")
        code, output = self.run_lint()
        self.assertEqual(code, 1, output)
        self.assertIn("does not match the file name 'spot-renamed-pg'", output)

    def test_an_unknown_allow_field_fails_so_a_typo_cannot_hide_a_missing_task(self) -> None:
        self.rust("CREATE TABLE legacy (id integer)")
        self.write(
            "allow/spot-demo-pg.toml",
            'crate = "spot-demo-pg"\nreason = "rename pending"\nrename_taks = "WP-X.1"\n',
        )
        code, output = self.run_lint()
        self.assertEqual(code, 1, output)
        self.assertIn("unknown fields ['rename_taks']", output)

    def test_unparseable_allow_toml_is_an_input_error(self) -> None:
        self.write("allow/spot-demo-pg.toml", "crate = \n")
        code, output = self.run_lint()
        self.assertEqual(code, 2, output)

    def test_missing_allow_directory_allows_nothing(self) -> None:
        self.rust("CREATE TABLE legacy (id integer)")
        self.allow.rmdir()
        self.assertEqual(self.run_lint()[0], 1)


class TreeTests(TempTreeTestCase):
    def test_a_missing_crates_directory_is_successful(self) -> None:
        (self.root / "crates" / "backends").rmdir()
        (self.root / "crates").rmdir()
        code, output = self.run_lint()
        self.assertEqual(code, 0, output)
        self.assertIn("no crates/backends/ directory", output)

    def test_a_crate_without_a_src_directory_is_skipped(self) -> None:
        (self.root / "crates" / "backends" / "spot-empty-pg").mkdir()
        self.assertEqual(self.run_lint()[0], 0)

    def test_the_shipped_allow_list_never_gains_a_crate(self) -> None:
        """The allow-list may only shrink: each rename task deletes its own file.

        KNOWN_LEGACY_CRATES is the closed set of the eight crates that were
        allowed when the gate shipped. A file for any other crate means a new
        crate was added to the allow-list, which the gate exists to stop.
        """
        shipped = {path.stem for path in (table_prefix_lint.HERE / "allow").glob("*.toml")}
        self.assertLessEqual(shipped, KNOWN_LEGACY_CRATES, "a crate was added to the allow-list")
        self.assertLessEqual(len(shipped), len(KNOWN_LEGACY_CRATES))

    def test_the_shipped_allow_entries_name_a_rename_task_and_a_reason(self) -> None:
        """Every entry still present names its rename task and a real reason (any count, down to none)."""
        backends = REPO_ROOT / "crates" / "backends"
        entries, problems = table_prefix_lint.load_allowlist(table_prefix_lint.HERE / "allow", backends)
        self.assertEqual(problems, [])
        self.assertLessEqual(len(entries), len(KNOWN_LEGACY_CRATES))
        for entry in entries:
            self.assertIn(entry.crate, KNOWN_LEGACY_CRATES, entry)
            self.assertTrue(entry.rename_task.startswith("WP-"), entry)
            self.assertGreater(len(entry.reason), 20, entry)

    def test_the_real_tree_passes(self) -> None:
        output = io.StringIO()
        code = table_prefix_lint.run(REPO_ROOT, table_prefix_lint.HERE / "allow", output=output)
        self.assertEqual(code, 0, output.getvalue())
        self.assertIn("clean:", output.getvalue())


if __name__ == "__main__":
    unittest.main()
