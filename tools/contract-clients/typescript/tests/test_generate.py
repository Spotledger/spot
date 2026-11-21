"""Unit tests for the TypeScript surface generator (WP-WEB-10.T6). No network."""

from __future__ import annotations

import hashlib
import os
import re
import shutil
import sys
import tempfile
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
TOOL = HERE.parent
sys.path.insert(0, str(TOOL))

import generate  # noqa: E402
import selftest  # noqa: E402
import tsescape  # noqa: E402

FIXTURES = HERE / "fixtures"
GOLDEN = HERE / "golden"
REAL = generate.DEFAULT_CONTRACT
UPDATE = os.environ.get("SPOT_UPDATE_GOLDEN") == "1"


def strip_ts(text: str) -> str:
    """Remove comments and string literals so only code is left."""
    out, i, n = [], 0, len(text)
    while i < n:
        two = text[i:i + 2]
        if two == "//":
            j = text.find("\n", i)
            i = n if j < 0 else j
        elif two == "/*":
            j = text.find("*/", i + 2)
            if j < 0:
                raise AssertionError("unterminated block comment")
            i = j + 2
        elif text[i] in "\"'`":
            quote, i = text[i], i + 1
            while i < n and text[i] != quote:
                if text[i] == "\n":
                    raise AssertionError("newline inside a string literal")
                i += 2 if text[i] == "\\" else 1
            if i >= n:
                raise AssertionError("unterminated string literal")
            i += 1
            out.append('""')
        else:
            out.append(text[i])
            i += 1
    return "".join(out)


def gen(name: str):
    return generate.generate(FIXTURES / name / "openapi.yaml")


class GoldenTests(unittest.TestCase):
    def check_fixture(self, name: str) -> None:
        files, _report = gen(name)
        for fname, text in sorted(files.items()):
            golden = GOLDEN / name / fname
            if UPDATE:
                golden.parent.mkdir(parents=True, exist_ok=True)
                golden.write_text(text, encoding="utf-8", newline="\n")
            self.assertTrue(golden.is_file(), f"missing golden {golden}; run with SPOT_UPDATE_GOLDEN=1")
            self.assertEqual(golden.read_text(encoding="utf-8"), text, f"{name}/{fname} differs from golden")

    def test_constructs_golden(self) -> None:
        self.check_fixture("constructs")

    def test_hostile_golden(self) -> None:
        self.check_fixture("hostile")

    def test_constructs_content(self) -> None:
        files, report = gen("constructs")
        types = files["types.ts"]
        self.assertIn("export interface Item {", types)
        self.assertIn("note?: string | null;", types)  # nullable, not required
        self.assertIn("kind?: \"item\";", types)  # const
        self.assertIn('export type Color = "red" | "green";', types)  # enum
        self.assertIn("export type Shape = Item | Bag;", types)  # oneOf
        self.assertIn("export type Combined = Item & {", types)  # allOf
        self.assertIn("[key: string]: number", types)  # additionalProperties schema
        self.assertIn("created_at?: string;", types)  # date-time stays a string
        self.assertIn("export type ItemId = string;", types)
        self.assertIn("string | number | boolean | null", types)
        ops = files["operations.ts"]
        table = selftest.read_table(ops)
        self.assertEqual(sorted(table), ["health", "item_attach", "item_delete", "item_get", "item_replace"])
        get = table["item_get"]
        self.assertEqual(get["pathParams"], [{"name": "id", "required": True}])  # path-level parameter
        self.assertEqual(get["queryParams"], [{"array": True, "name": "fields", "required": False}])
        self.assertTrue(get["etag"])
        self.assertEqual([s["status"] for s in get["success"]], [200, 304])
        self.assertIsNone(get["success"][1]["contentType"])  # 304 without a body
        self.assertEqual(get["errors"], {"default": "ErrorEnvelope"})
        put = table["item_replace"]
        self.assertTrue(put["idempotent"])
        self.assertTrue(put["etag"])
        self.assertEqual(put["errors"]["422"], "ErrorEnvelope")
        self.assertEqual(table["item_attach"]["request"], {"contentType": "multipart/form-data", "type": "FormParts"})
        self.assertIn('| { status: 304; unexpected?: false; headers: ResponseHeaders; body: null }', ops)
        self.assertEqual(sum("APPROXIMATE" in a for a in report.approx), 2)  # not, propertyNames

    def test_client_one_method_per_operation(self) -> None:
        files, _ = gen("constructs")
        for oid in ("item_get", "item_replace", "item_delete", "item_attach", "health"):
            self.assertEqual(len(re.findall(rf"^  {oid}\(", files["client.ts"], flags=re.M)), 1)
        self.assertIn("this.transport.send(", files["client.ts"])
        self.assertNotIn("fetch(", files["client.ts"])


class SafetyTests(unittest.TestCase):
    def test_hostile_text_never_reaches_code(self) -> None:
        files, report = gen("hostile")
        self.assertFalse(report.failed)
        for name, text in files.items():
            if not name.endswith(".ts"):
                continue
            self.assertNotIn("*/ ; process.exit", text)
            self.assertNotIn("*/ process.exit", text)
            if name == "client.ts":
                continue  # the hand-written runtime has regex literals; it holds no contract text
            code = strip_ts(text)  # raises on an early comment end or a broken string
            self.assertNotIn("process", code, f"{name}: hostile text escaped into code")
            self.assertNotIn("exit(", code)
            self.assertNotIn("__proto__", code)
        types = files["types.ts"]
        self.assertIn('"__proto__"?: string;', types)
        self.assertIn('"a\\"b"?: string;', types)
        self.assertIn('"quote\'n\\nline"?: string;', types)
        self.assertIn('"line\\nbreak"', types)

    def test_helpers(self) -> None:
        self.assertEqual(tsescape.ts_key("plain_1"), "plain_1")
        self.assertEqual(tsescape.ts_key("with-dash"), '"with-dash"')
        self.assertEqual(tsescape.ts_key("__proto__"), '"__proto__"')
        self.assertEqual(tsescape.ts_string('a"\n '), '"a\\"\\n\\u2028"')
        self.assertEqual(tsescape.doc_comment("a */ b"), ["/** a *\\/ b */"])
        self.assertNotIn("*/", tsescape.inline_comment("x */ y")[2:-2])

    def test_integers_that_do_not_fit_are_refused(self) -> None:
        _files, report = gen("int64")
        self.assertTrue(report.failed)
        text = "\n".join(sorted(report.unrepresentable))
        self.assertIn("minor_units", text)
        self.assertIn("huge", text)
        self.assertNotIn("fine", text)

    def test_check_fails_on_unrepresentable(self) -> None:
        rc = generate.main(["--contract", str(FIXTURES / "int64" / "openapi.yaml"), "--check"])
        self.assertEqual(rc, 1)


class RealContractTests(unittest.TestCase):
    def test_real_contract_passes_check(self) -> None:
        rc = generate.main(["--check"])
        self.assertEqual(rc, 0)

    def test_counts_equal_the_contract(self) -> None:
        files, report = generate.generate(REAL)
        want = selftest.contract_counts(REAL)
        self.assertEqual(report.counts["operations"], want["operations"])
        self.assertEqual(report.counts["schemas"], want["schemas"])
        self.assertEqual(len(selftest.read_table(files["operations.ts"])), want["operations"])
        self.assertEqual(len(re.findall(r"^export (?:interface|type) ", files["types.ts"], flags=re.M)), want["schemas"])

    def test_contract_version_is_carried(self) -> None:
        files, _ = generate.generate(REAL)
        version = (REAL.parent / "VERSION").read_text().strip()
        self.assertIn(f'CONTRACT_VERSION = "{version}";', files["contract.ts"])
        digest = hashlib.sha256(REAL.read_bytes()).hexdigest()
        self.assertIn(digest, files["contract.ts"])
        for name in ("types.ts", "operations.ts", "client.ts", "index.ts"):
            self.assertIn(f"contract version {version}", files[name])
            self.assertIn("Generated, do not edit", files[name])

    def test_deterministic(self) -> None:
        first, _ = generate.generate(REAL)
        second, _ = generate.generate(REAL)
        self.assertEqual(first, second)
        with tempfile.TemporaryDirectory() as a, tempfile.TemporaryDirectory() as b:
            generate.write(first, Path(a))
            generate.write(second, Path(b))
            for name in first:
                self.assertEqual((Path(a) / name).read_bytes(), (Path(b) / name).read_bytes())


class MutationTests(unittest.TestCase):
    """The self-test must notice a wrong output (item 5)."""

    def setUp(self) -> None:
        self.tmp = Path(tempfile.mkdtemp(prefix="spot-ts-test-"))
        self.addCleanup(shutil.rmtree, self.tmp, True)
        self.contract = FIXTURES / "constructs" / "openapi.yaml"
        files, self.report = generate.generate(self.contract)
        self.out = self.tmp / "out"
        generate.write(files, self.out)

    def findings(self) -> list[str]:
        return selftest.run(self.contract, "1.2.3", self.out, self.report.counts)

    def edit(self, name: str, old: str, new: str) -> None:
        path = self.out / name
        text = path.read_text(encoding="utf-8")
        self.assertIn(old, text)
        path.write_text(text.replace(old, new, 1), encoding="utf-8")

    def test_clean_output_has_no_findings(self) -> None:
        self.assertEqual(self.findings(), [])

    def test_wrong_table_path(self) -> None:
        self.edit("operations.ts", '"path": "/health"', '"path": "/healthz"')
        self.assertTrue(any("health" in f and "path" in f for f in self.findings()))

    def test_wrong_table_parameter(self) -> None:
        self.edit("operations.ts", '"name": "fields"', '"name": "field"')
        self.assertTrue(any("item_get" in f and "queryParams" in f for f in self.findings()))

    def test_missing_client_method(self) -> None:
        self.edit("client.ts", "  item_delete(", "  item_removed(")
        found = self.findings()
        self.assertTrue(any("item_delete has 0 client methods" in f for f in found))

    def test_missing_type(self) -> None:
        self.edit("types.ts", "export type Color =", "export type Colour =")
        self.assertTrue(any("schema Color" in f for f in self.findings()))

    def test_wrong_version(self) -> None:
        self.edit("contract.ts", '"1.2.3"', '"9.9.9"')
        self.assertTrue(any("version" in f for f in self.findings()))

    def test_removing_an_operation_removes_it_from_the_output(self) -> None:
        import yaml

        doc = yaml.safe_load(self.contract.read_text())
        del doc["paths"]["/health"]
        copy_dir = self.tmp / "copy"
        copy_dir.mkdir()
        (copy_dir / "openapi.yaml").write_text(yaml.safe_dump(doc))
        (copy_dir / "VERSION").write_text("1.2.3\n")
        files, report = generate.generate(copy_dir / "openapi.yaml")
        self.assertNotIn("health", selftest.read_table(files["operations.ts"]))
        self.assertNotIn("  health(", files["client.ts"])
        self.assertEqual(report.counts["operations"], 4)

    def test_dangling_ref_is_reported(self) -> None:
        import yaml

        doc = yaml.safe_load(self.contract.read_text())
        doc["components"]["schemas"]["Item"]["properties"]["color"] = {"$ref": "#/components/schemas/Nope"}
        copy_dir = self.tmp / "dangling"
        copy_dir.mkdir()
        (copy_dir / "openapi.yaml").write_text(yaml.safe_dump(doc))
        (copy_dir / "VERSION").write_text("1.2.3\n")
        _files, report = generate.generate(copy_dir / "openapi.yaml")
        self.assertTrue(report.failed)
        self.assertIn("Nope", "\n".join(report.findings))


class TscTests(unittest.TestCase):
    def test_fixtures_type_check_when_tsc_exists(self) -> None:
        tsc = selftest.find_tsc(dict(os.environ))
        if tsc is None:
            self.skipTest("tsc not available")
        for name in ("constructs", "hostile"):
            files, _ = gen(name)
            with tempfile.TemporaryDirectory() as tmp:
                generate.write(files, Path(tmp))
                self.assertEqual(selftest.type_check(Path(tmp), tsc, False), [], name)


if __name__ == "__main__":
    unittest.main()
