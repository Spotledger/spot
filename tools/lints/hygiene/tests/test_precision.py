"""WP-TST-13.T3: low-information fingerprints and tracked-only scanning.

These tests pin the false-positive fixes: literal-only and period-dominated
windows yield no fingerprints at all (so boilerplate-only files cannot match),
copies that merely rename identifiers and change indentation still match, and
default scans cover only git-tracked (or staged) files.
"""

import os
import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

HYGIENE_DIR = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(HYGIENE_DIR))
import hygiene
import reference

from test_reference import ScanOutputHidesReferenceSource

MODULE = ScanOutputHidesReferenceSource.MODULE
K = hygiene.DEFAULT_K


def assertion_run_source(statements: int = 8) -> str:
    keys = ("alpha", "bravo", "charlie", "delta", "echo", "foxtrot", "golf", "hotel")
    return "\n".join(
        f'self.assertEqual(report["{key}"], {100 + number})'
        for number, key in enumerate(keys[:statements])
    )


class LiteralOnlyWindows(unittest.TestCase):
    """A tuple of invented syllables must not fingerprint (names.py pattern)."""

    def _syllable_tuple(self) -> str:
        syllables = (
            "qa", "zex", "wuv", "kif", "voz", "pel", "nug", "hax",
            "cew", "jud", "lym", "foq", "bip", "zot", "gan", "rix",
            "sed", "vok", "tul", "yam", "dip", "kex", "zun", "baf",
            "qol", "vim", "jeg", "pus", "tho", "wux", "cev", "dyl",
        )
        return "(\n" + ",\n".join(f'"{syllable}"' for syllable in syllables) + ",\n)\n"

    def test_syllable_tuple_yields_no_fingerprints(self):
        source = self._syllable_tuple()
        tokens = hygiene.tokens_for("names.py", source)
        self.assertGreater(len(tokens), K)
        self.assertEqual(hygiene.fingerprints_for("names.py", source), [])

    def test_long_repeated_string_stays_fingerprint_free(self):
        source = '"' + "data " * 40 + '"'
        self.assertEqual(hygiene.fingerprints_for("copy.py", source), [])


class PeriodDominatedWindows(unittest.TestCase):
    """Runs of one assertion shape must not fingerprint (datagen test pattern)."""

    def test_repeated_assertions_yield_no_fingerprints(self):
        source = assertion_run_source()
        tokens = hygiene.tokens_for("test.py", source)
        self.assertGreater(len(tokens), K)
        self.assertEqual(hygiene.fingerprints_for("test.py", source), [])

    def test_assertion_run_is_period_dominated(self):
        tokens = hygiene.tokens_for("test.py", assertion_run_source())
        values = [token.value for token in tokens]
        self.assertTrue(hygiene.window_is_low_information(values, 0, K))
        self.assertTrue(hygiene.window_is_low_information(values, 10, K))

    def test_period_over_low_info_max_period_is_kept(self):
        # 17 distinct values repeating: every period p <= 16 mismatches across
        # the phase boundary, so the window keeps being fingerprinted.
        names = (
            "alpha", "bravo", "charlie", "delta", "echo", "foxtrot", "golf",
            "hotel", "india", "juliett", "kilo", "lima", "mike", "november",
            "oscar", "papa", "quebec",
        )
        values = list(names) * 2
        self.assertEqual(len(values), 34)
        self.assertFalse(hygiene.window_is_low_information(values, 0, K))
        self.assertFalse(hygiene.window_is_low_information(values, 2, K))

    def test_literal_only_target_file_does_not_crash_the_scan(self):
        with tempfile.TemporaryDirectory() as name:
            root = Path(name)
            reference_dir = root / "reference"
            target_dir = root / "targets"
            reference_dir.mkdir()
            target_dir.mkdir()
            (reference_dir / "real.py").write_text(MODULE, encoding="utf-8")
            (target_dir / "syllables.py").write_text(
                '(\n"qa",\n"zex",\n"wuv",\n"kif",\n"voz",\n"pel",\n"nug",\n"hax",\n'
                '"cew",\n"jud",\n"lym",\n"foq",\n"bip",\n"zot",\n"gan",\n"rix",\n'
                '"sed",\n"vok",\n"tul",\n"yam",\n"dip",\n"kex",\n"zun",\n"baf",\n'
                '"qol",\n"vim",\n"jeg",\n"pus",\n"tho",\n"wux",\n"cev",\n"dyl",\n)\n',
                encoding="utf-8",
            )
            index = root / "index.bin"
            hygiene.build_index(reference_dir, index)
            self.assertEqual(hygiene.scan_targets(index, target_dir), [])


class RealContentStillMatches(unittest.TestCase):
    """A mixed, realistic file keeps fingerprints and traces renamed copies."""

    def test_mixed_function_keeps_fingerprints(self):
        self.assertGreaterEqual(len(hygiene.fingerprints_for("ledger.py", MODULE)), 12)

    def test_renamed_copy_traces_back_to_the_reference(self):
        with tempfile.TemporaryDirectory() as name:
            root = Path(name)
            reference_dir = root / "reference"
            target_dir = root / "targets"
            reference_dir.mkdir()
            target_dir.mkdir()
            (reference_dir / "ledger.py").write_text(MODULE, encoding="utf-8")
            index = root / "index.bin"
            hygiene.build_index(reference_dir, index)
            renamed = reference._rename_transform(MODULE, "python")
            (target_dir / "renamed_ledger.py").write_text(renamed, encoding="utf-8")
            reports = hygiene.scan_targets(index, target_dir)
            self.assertTrue(reports)
            cited = {ref["path"] for report in reports for ref in report["references"]}
            self.assertIn("ledger.py", cited)

    def test_assertion_run_window_inside_a_real_file_is_dropped(self):
        # Real content plus a boilerplate run: windows that start inside the
        # assertion run stay low-information even in a mixed file.
        combined = MODULE + "\n" + assertion_run_source() + "\n"
        tokens = hygiene.tokens_for("mixed.py", combined)
        module_lines = MODULE.count("\n") + 1
        values = []
        run_start = None
        for position, token in enumerate(tokens):
            if token.line > module_lines and run_start is None:
                run_start = position
            values.append(token.value)
        self.assertIsNotNone(run_start)
        self.assertLessEqual(run_start + K, len(values))
        self.assertTrue(hygiene.window_is_low_information(values, run_start, K))


SYLLABLES = (
    "(\n"
    + "".join(f'    "{a}{b}",\n' for a in "qzwkvpnh" for b in "aeo")
    + ")\n"
)


def mixed_module(boilerplate: str) -> str:
    """MODULE with boilerplate inserted between its top-level definitions."""
    first, rest = MODULE.split("\n\n\ndef reconcile", 1)
    second, third = ("\n\n\ndef reconcile" + rest).split("\n\n\nclass PlanBuilder", 1)
    return (
        first + "\n\n\n" + boilerplate + "\n" + second
        + "\n\n\n" + boilerplate + "\n\n\nclass PlanBuilder" + third
    )


class MixedFilesStillMatch(unittest.TestCase):
    """Real code that embeds boilerplate is still caught via its real parts."""

    VARIANTS = {
        "literal list": "SYLLABLES = " + SYLLABLES,
        "assertion run": "def check(self, report):\n"
        + "\n".join("    " + line for line in assertion_run_source().splitlines()),
        "both": "SYLLABLES = " + SYLLABLES + "\n\n\ndef check(self, report):\n"
        + "\n".join("    " + line for line in assertion_run_source().splitlines()),
    }

    def _scan_renamed_copy(self, source: str) -> list[dict]:
        with tempfile.TemporaryDirectory() as name:
            root = Path(name)
            reference_dir = root / "reference"
            target_dir = root / "targets"
            reference_dir.mkdir()
            target_dir.mkdir()
            (reference_dir / "mixed_ref.py").write_text(source, encoding="utf-8")
            index = root / "index.bin"
            hygiene.build_index(reference_dir, index)
            renamed = reference._rename_transform(source, "python")
            (target_dir / "mixed_copy.py").write_text(renamed, encoding="utf-8")
            return hygiene.scan_targets(index, target_dir)

    def test_renamed_copy_is_reported_for_each_variant(self):
        for label, boilerplate in self.VARIANTS.items():
            with self.subTest(label):
                reports = self._scan_renamed_copy(mixed_module(boilerplate))
                self.assertTrue(reports)
                cited = {ref["path"] for r in reports for ref in r["references"]}
                self.assertIn("mixed_ref.py", cited)

    def test_mixed_file_fingerprints_both_sides_of_the_boilerplate(self):
        for label, boilerplate in self.VARIANTS.items():
            with self.subTest(label):
                source = mixed_module(boilerplate)
                spans = hygiene.fingerprints_for("mixed.py", source)
                real_only = hygiene.fingerprints_for("real.py", MODULE)
                self.assertGreaterEqual(len(spans), len(real_only))
                boiler_lines = [
                    number
                    for number, line in enumerate(source.splitlines(), 1)
                    if "SYLLABLES" in line or "def check" in line
                ]
                first_boiler, last_boiler = min(boiler_lines), max(boiler_lines)
                lines = [span[2] for span in spans]
                self.assertTrue(any(line < first_boiler for line in lines))
                self.assertTrue(any(line > last_boiler for line in lines))


def _git_env() -> dict:
    env = dict(os.environ)
    env.update(
        {
            "GIT_AUTHOR_NAME": "t",
            "GIT_AUTHOR_EMAIL": "t@example.com",
            "GIT_COMMITTER_NAME": "t",
            "GIT_COMMITTER_EMAIL": "t@example.com",
            "HOME": str(tempfile.gettempdir()),
        }
    )
    return env


def _git(repo: Path, *args: str) -> None:
    subprocess.run(
        ("git", "-C", str(repo), "-c", "commit.gpgsign=false", *args),
        check=True,
        capture_output=True,
        env=_git_env(),
    )


def _mini_index(root: Path) -> Path:
    """Index holding one realistic reference file, for target-dir scans."""
    origin = root / "origin"
    origin.mkdir()
    (origin / "ledger.py").write_text(MODULE, encoding="utf-8")
    index = root / "index.bin"
    hygiene.build_index(origin, index)
    return index


class TrackedFilesOnly(unittest.TestCase):
    """Default scans cover git-tracked (and staged) files, untracked never."""

    def setUp(self):
        if shutil.which("git") is None:
            self.skipTest("git not available")

    def test_untracked_files_are_not_scanned_by_default(self):
        with tempfile.TemporaryDirectory() as name:
            root = Path(name)
            repo = root / "repo"
            repo.mkdir()
            _git(repo, "init", "-q", ".")
            (repo / "tracked_module.py").write_text(MODULE, encoding="utf-8")
            _git(repo, "add", "tracked_module.py")
            _git(repo, "commit", "-q", "-m", "first")
            (repo / "untracked_module.py").write_text(MODULE, encoding="utf-8")
            index = _mini_index(root)
            reports = hygiene.scan_targets(index, repo)
            self.assertEqual(
                [report["target"] for report in reports], ["tracked_module.py"]
            )

    def test_staged_files_are_scanned_too(self):
        with tempfile.TemporaryDirectory() as name:
            root = Path(name)
            repo = root / "repo"
            repo.mkdir()
            _git(repo, "init", "-q", ".")
            (repo / "tracked_module.py").write_text(MODULE, encoding="utf-8")
            _git(repo, "add", "tracked_module.py")
            _git(repo, "commit", "-q", "-m", "first")
            (repo / "staged_module.py").write_text(MODULE, encoding="utf-8")
            _git(repo, "add", "staged_module.py")
            index = _mini_index(root)
            reports = hygiene.scan_targets(index, repo)
            self.assertEqual(
                sorted(report["target"] for report in reports),
                ["staged_module.py", "tracked_module.py"],
            )

    def test_all_files_flag_scans_untracked_files_too(self):
        with tempfile.TemporaryDirectory() as name:
            root = Path(name)
            repo = root / "repo"
            repo.mkdir()
            _git(repo, "init", "-q", ".")
            (repo / "tracked_module.py").write_text(MODULE, encoding="utf-8")
            _git(repo, "add", "tracked_module.py")
            _git(repo, "commit", "-q", "-m", "first")
            (repo / "untracked_module.py").write_text(MODULE, encoding="utf-8")
            index = _mini_index(root)
            reports = hygiene.scan_targets(index, repo, all_files=True)
            self.assertEqual(
                sorted(report["target"] for report in reports),
                ["tracked_module.py", "untracked_module.py"],
            )

    def test_non_git_directories_fall_back_to_the_file_walk(self):
        with tempfile.TemporaryDirectory() as name:
            root = Path(name)
            (root / "plain").mkdir()
            (root / "plain" / "module.py").write_text(MODULE, encoding="utf-8")
            files = list(hygiene.iter_target_files(root / "plain"))
            self.assertEqual([path.name for path in files], ["module.py"])


class LowInformationHelpers(unittest.TestCase):
    """Direct checks of the low-information window predicate."""

    def test_window_of_pure_literals_and_punctuation_is_dropped(self):
        values = ["LIT", ",", "[", "]", ",", "LIT", ",", "[", "]", ","] * 4
        self.assertTrue(hygiene.window_is_low_information(values, 0, K))

    def test_json_structure_only_window_is_dropped(self):
        values = ["JSON_ARRAY", "JSON_ITEM", "LIT", "LIT", "END_ARRAY", "END_OBJECT"] * 5
        self.assertTrue(hygiene.window_is_low_information(values, 0, K))

    def test_realistic_window_is_kept(self):
        values = [token.value for token in hygiene.tokens_for("ledger.py", MODULE)]
        self.assertFalse(hygiene.window_is_low_information(values, 0, K))


if __name__ == "__main__":
    unittest.main()