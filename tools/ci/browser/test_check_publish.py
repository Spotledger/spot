import json
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import check_publish as cp  # noqa: E402


def write(root: Path, rel: str, data: bytes):
    path = root / rel
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(data)


def make_build(root: Path, files: dict):
    for rel, data in files.items():
        write(root, rel, data)
    entries = [{"path": rel, "bytes": len(data), "sha256": cp.sha256_file(root / rel)}
               for rel, data in sorted(files.items())]
    manifest = {"version": 1, "build": cp.build_id(entries), "files": entries}
    (root / "assets.json").write_text(json.dumps(manifest), encoding="utf-8")


GOOD = {
    "_framework/dotnet.js": b"js" * 50,
    "_framework/dotnet.js.br": b"b" * 10,
    "_framework/dotnet.js.gz": b"g" * 12,
    "_framework/app.wasm": b"w" * 500,
    "_framework/app.wasm.br": b"b" * 100,
    "_framework/app.wasm.gz": b"g" * 120,
    "index.html": b"<html></html>",
}


class CheckPublish(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name) / "wwwroot"
        self.root.mkdir()
        self.roots = Path(self.tmp.name) / "linker-roots.xml"
        self.roots.write_text("<linker/>")
        self.budgets = Path(self.tmp.name) / "budgets.toml"
        self.budgets.write_text('[[budget]]\nid = "browser.download.compressed"\nmax = 100000\n')

    def run_check(self, budgets="default", roots="default"):
        return cp.check(self.root,
                        self.budgets if budgets == "default" else budgets,
                        self.roots if roots == "default" else roots)

    def test_good_build_passes_and_counts_brotli(self):
        make_build(self.root, GOOD)
        errors, report = self.run_check()
        self.assertEqual(errors, [])
        # .br where it exists (10 + 100) plus index.html (13) and the manifest itself;
        # the .gz files are not extra.
        self.assertEqual(report["compressed_total_bytes"], 123 + (self.root / "assets.json").stat().st_size)

    def test_missing_manifest(self):
        errors, _ = self.run_check()
        self.assertTrue(any("assets.json is missing" in e for e in errors))

    def test_missing_folder(self):
        errors, _ = cp.check(self.root / "nope", None, None)
        self.assertTrue(any("not found" in e for e in errors))

    def test_listed_file_missing(self):
        make_build(self.root, GOOD)
        (self.root / "index.html").unlink()
        errors, _ = self.run_check()
        self.assertTrue(any("listed file is missing: index.html" in e for e in errors))

    def test_hash_mismatch(self):
        make_build(self.root, GOOD)
        (self.root / "index.html").write_bytes(b"<html>X</html>")
        errors, _ = self.run_check()
        self.assertTrue(any("hash differs for index.html" in e for e in errors))
        self.assertTrue(any("size differs for index.html" in e for e in errors))

    def test_unlisted_file(self):
        make_build(self.root, GOOD)
        write(self.root, "extra.txt", b"x")
        errors, _ = self.run_check()
        self.assertTrue(any("not listed" in e and "extra.txt" in e for e in errors))

    def test_manifest_listing_itself(self):
        make_build(self.root, GOOD)
        manifest = json.loads((self.root / "assets.json").read_text())
        manifest["files"].insert(0, {"path": "assets.json", "bytes": 1, "sha256": "0" * 64})
        (self.root / "assets.json").write_text(json.dumps(manifest))
        errors, _ = self.run_check()
        self.assertTrue(any("must not list itself" in e for e in errors))

    def test_wrong_build_id(self):
        make_build(self.root, GOOD)
        manifest = json.loads((self.root / "assets.json").read_text())
        manifest["build"] = "a" * 64
        (self.root / "assets.json").write_text(json.dumps(manifest))
        errors, _ = self.run_check()
        self.assertTrue(any("'build' does not match" in e for e in errors))

    def test_missing_brotli_sibling(self):
        files = dict(GOOD)
        del files["_framework/app.wasm.br"]
        make_build(self.root, files)
        errors, _ = self.run_check()
        self.assertTrue(any("no Brotli sibling for _framework/app.wasm" in e for e in errors))

    def test_no_brotli_at_all(self):
        make_build(self.root, {"index.html": b"<html></html>"})
        errors, _ = self.run_check()
        self.assertTrue(any("no .br file" in e for e in errors))

    def test_budget_exceeded(self):
        make_build(self.root, GOOD)
        self.budgets.write_text('[[budget]]\nid = "browser.download.compressed"\nmax = 100\n')
        errors, _ = self.run_check()
        self.assertTrue(any("exceeds budget" in e for e in errors))

    def test_budget_absent_is_not_an_error(self):
        make_build(self.root, GOOD)
        errors, report = self.run_check(budgets=self.root / "no-such.toml")
        self.assertEqual(errors, [])
        self.assertIsNone(report["budget"])

    def test_linker_roots_missing(self):
        make_build(self.root, GOOD)
        errors, _ = self.run_check(roots=Path(self.tmp.name) / "absent.xml")
        self.assertTrue(any("linker-roots file is missing" in e for e in errors))

    def test_bad_manifest_json(self):
        (self.root / "assets.json").write_text("{nope")
        errors, _ = self.run_check()
        self.assertTrue(any("not valid JSON" in e for e in errors))

    def test_main_writes_report_and_exit_code(self):
        make_build(self.root, GOOD)
        out = Path(self.tmp.name) / "r" / "size.json"
        code = cp.main([str(self.root), "--budgets", str(self.budgets),
                        "--linker-roots", str(self.roots), "--report", str(out)])
        self.assertEqual(code, 0)
        self.assertGreater(json.loads(out.read_text())["compressed_total_bytes"], 123)
        (self.root / "index.html").unlink()
        self.assertEqual(cp.main([str(self.root), "--budgets", str(self.budgets),
                                  "--linker-roots", str(self.roots)]), 1)

    def test_real_defaults_exist(self):
        self.assertTrue(cp.DEFAULT_LINKER_ROOTS.is_file())
        self.assertIsNotNone(cp.load_budget(cp.DEFAULT_BUDGETS))


if __name__ == "__main__":
    unittest.main()
