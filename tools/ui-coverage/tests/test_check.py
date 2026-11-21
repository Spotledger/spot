import json
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))
import check  # noqa: E402

MANIFEST = """
[[screen]]
id = "a.form"
title = "A form"
module = "a"
kind = "form"
design_ref = "d#a"
required_states = ["default", "error"]
priority = "p0"

[[screen]]
id = "a.list"
title = "A list"
module = "a"
kind = "list"
design_ref = "d#l"
priority = "p1"

[[screen]]
id = "b.rep"
title = "B report"
module = "b"
kind = "report"
design_ref = "d#r"
priority = "p2"
"""


class Tree(unittest.TestCase):
    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory()
        self.root = Path(self._tmp.name)
        self.addCleanup(self._tmp.cleanup)
        (self.root / "screens").mkdir()
        (self.root / "src").mkdir()
        (self.root / "tests").mkdir()
        self.write("screens/m.toml", MANIFEST)

    def write(self, rel, text):
        p = self.root / rel
        p.parent.mkdir(parents=True, exist_ok=True)
        p.write_text(text, encoding="utf-8")

    def run_cli(self, *args):
        argv = ["--screens", str(self.root / "screens"), "--src", str(self.root / "src"),
                "--tests", str(self.root / "tests"), *args]
        import contextlib, io
        with contextlib.redirect_stdout(io.StringIO()):
            return check.main(argv)


class ManifestTests(Tree):
    def test_loads_and_ignores_examples(self):
        self.write("screens/example.toml", MANIFEST.replace("a.form", "x.form"))
        self.assertEqual(sorted(check.load_manifest(self.root / "screens")), ["a.form", "a.list", "b.rep"])

    def test_duplicate_id_rejected(self):
        self.write("screens/n.toml", MANIFEST.split("[[screen]]")[1].join(["[[screen]]", ""]))
        with self.assertRaises(check.InputError):
            check.load_manifest(self.root / "screens")

    def test_bad_values_rejected(self):
        for bad in ('kind = "form"', 'priority = "p0"', 'id = "a.form"'):
            self.write("screens/m.toml", MANIFEST.replace(bad, bad.split("=")[0] + '= "BAD value"', 1))
            with self.assertRaises(check.InputError, msg=bad):
                check.load_manifest(self.root / "screens")

    def test_unknown_key_rejected(self):
        self.write("screens/m.toml", MANIFEST.replace('priority = "p2"', 'priority = "p2"\nowner = "x"'))
        with self.assertRaises(check.InputError):
            check.load_manifest(self.root / "screens")

    def test_missing_dir(self):
        with self.assertRaises(check.InputError):
            check.load_manifest(self.root / "nope")
        self.assertEqual(self.run_cli("--screens", str(self.root / "nope")), 2)


class ScanTests(Tree):
    def test_scan_implemented_and_covered(self):
        self.write("src/X/A.cs", '[ScreenId("a.form")] class A {}\n[Spotledger.Client.Analytics.ScreenIdAttribute("a.list")] class B {}')
        self.write("tests/T/A.cs", '[CoversScreen("a.form", "default", "error")] void t() {}\n[CoversScreen("a.list")] void u() {}')
        impl = check.scan_implemented(self.root / "src")
        cov = check.scan_covered(self.root / "tests")
        self.assertEqual(sorted(impl), ["a.form", "a.list"])
        self.assertEqual(cov["a.form"]["states"], {"default", "error"})
        self.assertEqual(cov["a.list"]["states"], {"default"})

    def test_comments_and_build_dirs_ignored(self):
        self.write("src/X/A.cs", '// [ScreenId("a.form")]\n/* [ScreenId("a.list")] */\nclass A {}')
        self.write("src/X/obj/G.cs", '[ScreenId("b.rep")] class G {}')
        self.assertEqual(check.scan_implemented(self.root / "src"), {})

    def test_string_literal_containing_comment_marker_kept(self):
        self.write("src/X/A.cs", 'var u = "http://x"; [ScreenId("a.form")] class A {}')
        self.assertEqual(list(check.scan_implemented(self.root / "src")), ["a.form"])

    def test_analytics_tests_excluded_by_default(self):
        self.write("tests/Spotledger.Client.Analytics.Tests/F.cs", '[CoversScreen("a.form")] class F {}')
        self.assertEqual(check.scan_covered(self.root / "tests"), {})

    def test_missing_roots_are_empty(self):
        self.assertEqual(check.scan_implemented(self.root / "nope"), {})


class ReportTests(Tree):
    def evaluate(self):
        m = check.load_manifest(self.root / "screens")
        return check.evaluate(m, check.scan_implemented(self.root / "src"), check.scan_covered(self.root / "tests"))

    def test_coverage_counts_and_missing_states(self):
        self.write("src/A.cs", '[ScreenId("a.form")] class A {}\n[ScreenId("a.list")] class B {}')
        self.write("tests/A.cs", '[CoversScreen("a.form", "default")] void t() {}')
        r = self.evaluate()
        self.assertEqual(r["totals"], {"declared": 3, "implemented": 2, "covered": 1, "covered_for_states": 0})
        self.assertEqual(r["by_module"]["a"]["implemented"], 2)
        self.assertEqual(r["by_kind"]["report"]["declared"], 1)
        row = {x["id"]: x for x in r["screens"]}
        self.assertEqual(row["a.form"]["missing_states"], ["error"])
        self.assertEqual([x["id"] for x in r["missing"]], ["a.form", "a.list", "b.rep"])

    def test_full_coverage(self):
        self.write("src/A.cs", '[ScreenId("a.form")][ScreenId("a.list")][ScreenId("b.rep")] class A {}')
        self.write("tests/A.cs", '[CoversScreen("a.form", "default", "error")][CoversScreen("a.list")][CoversScreen("b.rep")] class T {}')
        r = self.evaluate()
        self.assertEqual(r["missing"], [])
        self.assertEqual(self.run_cli("--min-implemented", "100", "--min-covered", "100"), 0)

    def test_unknown_ids_reported(self):
        self.write("src/A.cs", '[ScreenId("zzz")] class A {}')
        self.write("tests/A.cs", '[CoversScreen("typo")] class T {}')
        r = self.evaluate()
        self.assertEqual(r["unknown_implemented"], ["zzz"])
        self.assertEqual(r["unknown_covered"], ["typo"])
        self.assertEqual(self.run_cli(), 1)  # a test naming an unknown screen fails

    def test_thresholds(self):
        self.write("src/A.cs", '[ScreenId("a.form")] class A {}')
        self.assertEqual(self.run_cli(), 0)
        self.assertEqual(self.run_cli("--min-implemented", "33"), 0)
        self.assertEqual(self.run_cli("--min-implemented", "33", "--min-covered", "1"), 1)
        self.assertEqual(self.run_cli("--min-implemented", "50"), 1)

    def test_outputs(self):
        self.run_cli("--json", str(self.root / "r.json"), "--markdown", str(self.root / "r.md"))
        data = json.loads((self.root / "r.json").read_text())
        self.assertEqual(data["totals"]["declared"], 3)
        self.assertIn("| a.form |", (self.root / "r.md").read_text())
        self.assertIn("d#a", (self.root / "r.md").read_text())

    def test_found_json_merges(self):
        self.write("found.json", json.dumps({"implemented": {"a.list": "N.T"}, "covered": {"a.list": {"states": ["default"]}}}))
        self.run_cli("--found", str(self.root / "found.json"), "--json", str(self.root / "r.json"))
        row = {x["id"]: x for x in json.loads((self.root / "r.json").read_text())["screens"]}
        self.assertTrue(row["a.list"]["implemented"] and row["a.list"]["tested"])

    def test_bad_found_json(self):
        self.write("found.json", "{")
        self.assertEqual(self.run_cli("--found", str(self.root / "found.json")), 2)


class BaselineTests(Tree):
    def test_regressions_only_flow(self):
        # today: nothing implemented. Record the baseline: existing gaps do not block.
        self.assertEqual(self.run_cli("--write-baseline"), 0)
        self.assertEqual(self.run_cli("--regressions-only"), 0)
        # implement and fully test one screen: still fine
        self.write("src/A.cs", '[ScreenId("a.list")] class A {}')
        self.write("tests/A.cs", '[CoversScreen("a.list")] class T {}')
        self.assertEqual(self.run_cli("--regressions-only"), 0)
        # refresh the baseline, then lose the test: a new gap
        self.assertEqual(self.run_cli("--write-baseline"), 0)
        (self.root / "tests/A.cs").unlink()
        self.assertEqual(self.run_cli("--regressions-only"), 1)
        # without --regressions-only the old gaps would not matter either way: plain run passes (no thresholds)
        self.assertEqual(self.run_cli(), 0)

    def test_new_screen_in_manifest_is_a_regression(self):
        self.assertEqual(self.run_cli("--write-baseline"), 0)
        self.write("screens/c.toml", MANIFEST.replace("a.form", "c.form").replace("a.list", "c.list").replace("b.rep", "c.rep"))
        self.assertEqual(self.run_cli("--regressions-only"), 1)

    def test_missing_state_regression(self):
        self.write("src/A.cs", '[ScreenId("a.form")] class A {}')
        self.write("tests/A.cs", '[CoversScreen("a.form", "default", "error")] class T {}')
        self.run_cli("--write-baseline")
        self.write("tests/A.cs", '[CoversScreen("a.form", "default")] class T {}')
        self.assertEqual(self.run_cli("--regressions-only"), 1)

    def test_bad_baseline(self):
        self.write("screens/baseline.json", "[1]")
        self.assertEqual(self.run_cli("--regressions-only"), 2)


if __name__ == "__main__":
    unittest.main()
