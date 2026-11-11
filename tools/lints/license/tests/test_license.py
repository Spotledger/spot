import json
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import check  # noqa: E402


def meta(root, pkgs):
    return {"packages": [{"name": n, "license": lic, "manifest_path": str(Path(root) / n / "Cargo.toml")} for n, lic in pkgs]}


class Packages(unittest.TestCase):
    def test_offenders_listed_together(self):
        with tempfile.TemporaryDirectory() as d:
            f = check.package_findings(meta(d, [("a", "MIT"), ("b", None), ("c", "Apache-2.0")]), Path(d), set())
            self.assertEqual(len(f), 2)
            self.assertTrue(any("a/Cargo.toml" in x for x in f))

    def test_clean_pass(self):
        with tempfile.TemporaryDirectory() as d:
            self.assertEqual(check.package_findings(meta(d, [("a", "Apache-2.0")]), Path(d), set()), [])

    def test_allow_list(self):
        with tempfile.TemporaryDirectory() as d:
            self.assertEqual(check.package_findings(meta(d, [("a", "MIT")]), Path(d), {"a"}), [])

    def test_outside_root_ignored(self):
        with tempfile.TemporaryDirectory() as d, tempfile.TemporaryDirectory() as other:
            self.assertEqual(check.package_findings(meta(other, [("a", "MIT")]), Path(d), set()), [])

    def test_read_list_strips_comments(self):
        with tempfile.TemporaryDirectory() as d:
            p = Path(d) / "l.txt"
            p.write_text("# header\nfoo  # why\n\n", encoding="utf-8")
            self.assertEqual(check.read_list(p), {"foo"})


class Marker(unittest.TestCase):
    def tree(self, d):
        (Path(d) / "x").mkdir()
        (Path(d) / "x" / "Cargo.toml").write_text(f'license = "{check.MARKER}"\n', encoding="utf-8")
        (Path(d) / "ok").mkdir()
        (Path(d) / "ok" / "Cargo.toml").write_text('license = "Apache-2.0"\n', encoding="utf-8")
        (Path(d) / "target").mkdir()
        (Path(d) / "target" / "Cargo.toml").write_text(check.MARKER, encoding="utf-8")

    def test_walk_fallback_finds_marker(self):
        with tempfile.TemporaryDirectory() as d:
            self.tree(d)
            f = check.marker_findings(Path(d), set())
            self.assertEqual(len(f), 1)
            self.assertIn("x/Cargo.toml", f[0])

    def test_proprietary_paths_list(self):
        with tempfile.TemporaryDirectory() as d:
            self.tree(d)
            self.assertEqual(check.marker_findings(Path(d), {"x/Cargo.toml"}), [])


class Cli(unittest.TestCase):
    def test_exit_codes_with_metadata_file(self):
        with tempfile.TemporaryDirectory() as d:
            m = Path(d) / "m.json"
            m.write_text(json.dumps(meta(d, [("a", "MIT")])), encoding="utf-8")
            argv = sys.argv
            try:
                sys.argv = ["check", "--root", d, "--metadata", str(m)]
                self.assertEqual(check.main(), 1)
                m.write_text(json.dumps(meta(d, [("a", "Apache-2.0")])), encoding="utf-8")
                self.assertEqual(check.main(), 0)
            finally:
                sys.argv = argv


if __name__ == "__main__":
    unittest.main()
