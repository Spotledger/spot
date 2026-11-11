from __future__ import annotations

import contextlib
import io
import shutil
import sys
import tempfile
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE.parent))
import error_codes_lint  # noqa: E402

FIXTURE_CATALOGUE = HERE / "fixtures" / "catalogue"
CLEAN_SRC = HERE / "fixtures" / "allowed_crate_src" / "clean.rs"
GAP_SRC = HERE / "fixtures" / "gap_crate_src" / "gap.rs"
LITERALS_DIR = HERE / "fixtures" / "literals_crate_src"


class ErrorCodesLintTests(unittest.TestCase):
    """The scan against a tiny fake source tree: the known gap fails, the clean tree passes."""

    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory()
        self.root = Path(self.temporary.name)

    def tearDown(self) -> None:
        self.temporary.cleanup()

    def make_tree(self, source: str) -> Path:
        """A minimal repo shape: one crate, one source file, the fixture catalogue."""
        (self.root / "crates" / "fake" / "src").mkdir(parents=True)
        (self.root / "crates" / "fake" / "src" / "lib.rs").write_text(source, encoding="utf-8")
        shutil.copytree(FIXTURE_CATALOGUE, self.root / "crates" / "framework" / "spot-errors" / "catalogue")
        return self.root

    def lint(self, root: Path) -> tuple[int, str]:
        output = io.StringIO()
        with contextlib.redirect_stdout(output), contextlib.redirect_stderr(output):
            result = error_codes_lint.run(root)
        return result, output.getvalue()

    def test_gap_fails_listing_file_and_line(self) -> None:
        source = GAP_SRC.read_text(encoding="utf-8")
        root = self.make_tree(source)
        code, output = self.lint(root)
        self.assertEqual(code, 1, output)
        self.assertIn("doc.missing", output)
        expected = next(
            i + 1
            for i, line in enumerate(source.splitlines())
            if '"doc.missing"' in line
        )
        self.assertIn(f"lib.rs:{expected}", output, "the listing names the file and line of the gap")
        self.assertIn("not catalogued", output)

    def test_clean_tree_passes(self) -> None:
        root = self.make_tree(CLEAN_SRC.read_text(encoding="utf-8"))
        code, output = self.lint(root)
        self.assertEqual(code, 0, output)
        self.assertIn("every raised or defined code is catalogued", output)

    def test_wrapper_calls_count_as_raises(self) -> None:
        source = CLEAN_SRC.read_text(encoding="utf-8")
        # Remove the direct constructor raise; the wrapper call must still count.
        without_direct = source.replace(
            'AppError::new(ErrorClass::Validation, "doc.thing", "The value is wrong.")',
            "unreachable!()",
        )
        root = self.make_tree(without_direct)
        code, output = self.lint(root)
        self.assertEqual(code, 0, output)

    def test_unraised_catalogue_entry_fails(self) -> None:
        source = CLEAN_SRC.read_text(encoding="utf-8")
        without_any = source.replace(
            'AppError::new(ErrorClass::Validation, "doc.thing", "The value is wrong.")',
            'AppError::new(ErrorClass::Validation, held, "x")',
        )
        without_any = without_any.replace('internal("doc.thing", msg)', 'internal(held, "x")')
        without_any = without_any.replace(
            "pub fn wrapped_raise() -> AppError {",
            'pub fn wrapped_raise() -> AppError {\n    let held = "doc.held";',
        )
        root = self.make_tree(without_any)
        code, output = self.lint(root)
        self.assertEqual(code, 1, output)
        self.assertIn("doc.thing is catalogued but no scanned source raises it", output)
        # The retired entry is exempt.
        self.assertNotIn("doc.retired", output)

    def test_retired_flag_exempts_unraised_entry(self) -> None:
        catalogue = (FIXTURE_CATALOGUE / "doc.toml").read_text(encoding="utf-8")
        marked = catalogue.replace(
            'code = "doc.thing"\nclass',
            'code = "doc.thing"\nretired = true\nclass',
        )
        root = self.make_tree("")  # nothing raises anything
        path = root / "crates" / "framework" / "spot-errors" / "catalogue" / "doc.toml"
        path.write_text(marked, encoding="utf-8")
        code, output = self.lint(root)
        self.assertEqual(code, 0, f"a retired entry is exempt from the unraised rule: {output}")

    def test_comments_are_masked(self) -> None:
        source = GAP_SRC.read_text(encoding="utf-8")
        commented = source.replace(
            'AppError::new(ErrorClass::Validation, "doc.missing", "The gap the tests look for.")',
            '// AppError::new(ErrorClass::Validation, "doc.missing", "commented out")',
        ).replace('internal("doc.thing", msg)', 'internal("// doc.thing", msg)')
        # doc.thing is still raised by mixed_raise's constructor, so the tree is complete:
        # the invariant is that the commented occurrences (doc.missing, the wrapper argument)
        # never count as raised or defined.
        root = self.make_tree(commented)
        code, output = self.lint(root)
        self.assertEqual(code, 0, f"the remaining raise keeps the tree complete: {output}")
        self.assertNotIn("doc.missing", output)
        self.assertNotIn("doc.comment_only", output)
        self.assertNotIn("any.doc_comment_style", output)
        self.assertNotIn("// doc.thing", output)

    def test_test_gated_raises_are_ignored(self) -> None:
        source = GAP_SRC.read_text(encoding="utf-8")
        without_gap = source.replace(
            'AppError::new(ErrorClass::Validation, "doc.missing", "The gap the tests look for.")',
            "// gap commented out",
        )
        root = self.make_tree(without_gap)
        code, output = self.lint(root)
        self.assertEqual(code, 0, f"the cfg(test) raise must not count: {output}")
        self.assertNotIn("doc.test_only", output)

    def test_toml_paths_are_not_codes(self) -> None:
        source = CLEAN_SRC.read_text(encoding="utf-8") + '\nconst CATALOGUE: &str = "doc.toml";\n'
        root = self.make_tree(source)
        code, output = self.lint(root)
        self.assertEqual(code, 0, output)

    def test_missing_catalogue_is_environment_trouble(self) -> None:
        root = self.make_tree(CLEAN_SRC.read_text(encoding="utf-8"))
        shutil.rmtree(root / "crates" / "framework" / "spot-errors" / "catalogue")
        code, output = self.lint(root)
        self.assertEqual(code, 2, output)

    def test_duplicate_catalogue_code_fails(self) -> None:
        catalogue = (FIXTURE_CATALOGUE / "doc.toml").read_text(encoding="utf-8")
        catalogued = catalogue + catalogue
        root = self.make_tree(CLEAN_SRC.read_text(encoding="utf-8"))
        path = root / "crates" / "framework" / "spot-errors" / "catalogue" / "doc.toml"
        path.write_text(catalogued, encoding="utf-8")
        code, output = self.lint(root)
        self.assertEqual(code, 1, output)
        self.assertIn("duplicates code", output)

    def test_tests_directory_is_not_scanned(self) -> None:
        source = CLEAN_SRC.read_text(encoding="utf-8")
        root = self.make_tree(source)
        tests_dir = root / "crates" / "fake" / "tests"
        tests_dir.mkdir()
        (tests_dir / "integration.rs").write_text(
            "use spot_app_api::{AppError, ErrorClass};\n"
            "pub fn raise() -> AppError {\n"
            '    AppError::new(ErrorClass::Internal, "tests.only", "never scanned")\n'
            "}\n",
            encoding="utf-8",
        )
        code, output = self.lint(root)
        self.assertEqual(code, 0, "tests/ trees are outside the scanned vocabulary")

    def test_entrypoints_are_not_scanned(self) -> None:
        source = CLEAN_SRC.read_text(encoding="utf-8")
        root = self.make_tree(source)
        (root / "crates" / "fake" / "src" / "main.rs").write_text(
            "fn main() {\n"
            '    let _ = "never.scanned";\n'
            "}\n",
            encoding="utf-8",
        )
        code, output = self.lint(root)
        self.assertEqual(code, 0, "main.rs entrypoints are outside the scanned vocabulary")

    def test_code_after_any_literal_form_is_still_found(self) -> None:
        """A raise after escapes, char literals, comment markers or lifetimes is not blanked."""
        fixtures = sorted(LITERALS_DIR.glob("*.rs"))
        self.assertGreaterEqual(len(fixtures), 8)
        for fixture in fixtures:
            with self.subTest(fixture=fixture.name):
                source = fixture.read_text(encoding="utf-8")
                with tempfile.TemporaryDirectory() as tmp:
                    root = Path(tmp)
                    (root / "crates" / "fake" / "src").mkdir(parents=True)
                    (root / "crates" / "fake" / "src" / "lib.rs").write_text(source, encoding="utf-8")
                    shutil.copytree(
                        FIXTURE_CATALOGUE, root / "crates" / "framework" / "spot-errors" / "catalogue"
                    )
                    code, output = self.lint(root)
                expected = next(
                    i + 1 for i, line in enumerate(source.splitlines()) if '"doc.missing"' in line
                )
                self.assertEqual(code, 1, output)
                self.assertIn(f"lib.rs:{expected}: doc.missing", output)

    def test_literal_helper_reads_each_form_as_one_unit(self) -> None:
        lit = error_codes_lint.literal_at
        self.assertEqual(lit('"a\\"b" tail', 0), ("string", 6))
        self.assertEqual(lit('"a\\\\" tail', 0), ("string", 5))
        self.assertEqual(lit("'\"' x", 0), ("char", 3))
        self.assertEqual(lit("'\\'' x", 0), ("char", 4))
        self.assertEqual(lit("'\\\\' x", 0), ("char", 4))
        self.assertEqual(lit("'\\u{1F600}' x", 0), ("char", 11))
        self.assertIsNone(lit("'a str", 0), "a lifetime is not a literal")
        self.assertIsNone(lit("'static>", 0))
        self.assertEqual(lit('r#"x"y"# z', 0), ("raw", 8))
        self.assertEqual(lit('br"x" z', 1), ("raw", 5))
        self.assertIsNone(lit('bar"x"', 2), "an identifier ending in r is not a raw string")

    def test_code_in_a_raw_string_is_not_counted(self) -> None:
        source = (
            "use spot_app_api::{AppError, ErrorClass};\n"
            'pub const DOC: &str = r#"AppError::new(ErrorClass::Validation, "doc.rawonly", "x")"#;\n'
            'pub const SAY: &str = "say \\"hi\\"";\n'
            "pub fn raise() -> AppError {\n"
            '    AppError::new(ErrorClass::Validation, "doc.thing", "The value is wrong.")\n'
            "}\n"
        )
        root = self.make_tree(source)
        code, output = self.lint(root)
        self.assertEqual(code, 0, output)
        self.assertNotIn("doc.rawonly", output)

    def test_comma_inside_a_literal_does_not_split_arguments(self) -> None:
        parts = error_codes_lint.split_args('"a,b", \',\', code')
        self.assertEqual(len(parts), 3)

    def test_known_non_code_literals_are_not_defined_codes(self) -> None:
        source = CLEAN_SRC.read_text(encoding="utf-8") + (
            '\nconst BUNDLE: &str = "app.wasm";\nconst SCRIPT: &str = "script.ts";\n'
            'const DOM: &str = "document.cookie";\n'
        )
        root = self.make_tree(source)
        code, output = self.lint(root)
        self.assertEqual(code, 0, output)

    def test_document_span_names_are_exempt_but_near_misses_are_not(self) -> None:
        exempt = ("doc.op", "doc.phase", "doc.step")
        for name in exempt:
            self.assertIn(name, error_codes_lint.NON_CODE_LITERALS)
        source = CLEAN_SRC.read_text(encoding="utf-8") + "".join(
            f'\nconst S{i}: &str = "{name}";' for i, name in enumerate(exempt)
        )
        code, output = self.lint(self.make_tree(source))
        self.assertEqual(code, 0, output)
        for near in ("doc.ops", "doc.opx", "doc.phases", "doc.steps", "xdoc.op", "doc.op.x"):
            self.assertNotIn(near, error_codes_lint.NON_CODE_LITERALS)
            shutil.rmtree(self.root / "crates")
            source = CLEAN_SRC.read_text(encoding="utf-8") + f'\nconst N: &str = "{near}";'
            code, output = self.lint(self.make_tree(source))
            self.assertNotEqual(code, 0, near)
            self.assertIn(near, output)


if __name__ == "__main__":
    unittest.main()