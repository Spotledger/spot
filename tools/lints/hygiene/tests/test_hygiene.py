import os
import random
import sys
import tempfile
import time
import unittest
from pathlib import Path

HYGIENE_DIR = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(HYGIENE_DIR))
import hygiene

CORPUS = Path(__file__).with_name("corpus")
REFERENCE = CORPUS / "reference"
TARGETS = CORPUS / "targets"

POSITIVE_TARGETS = {
    "python/renamed_copy.py",
    "javascript/renamed_timer.js",
    "typescript/renamed_ticket.ts",
    "rust/function_fragment.rs",
    "csharp/TrendCopy.cs",
    "json/panel_values_changed.json",
}

INDEPENDENT_NEGATIVES = {
    "python/independent.py",
    "javascript/independent_grid.js",
    "rust/independent_checksum.rs",
    "csharp/independent_stats.cs",
}


def write_synthetic_python_corpus(
    root: Path, total_bytes: int = 50 * 1024 * 1024, seed: int = 20260928
) -> tuple[int, str]:
    """Emit token-dense (comment-free) Python sources of varied structure until total_bytes.

    Statements are drawn from templates with randomised operators, comparison
    kinds, loop shapes and chain lengths, so consecutive k-grams differ and the
    fingerprint table is not degenerate duplicates. Returns (file count, text
    of the last module) so callers can seed a renamed probe target.
    """
    rng = random.Random(seed)
    operators = ["+", "-", "*", "//"]
    comparisons = ["<", ">", "<=", ">=", "==", "!="]
    written = 0
    module_index = 0
    last_text = ""
    while written < total_bytes:
        lines = []
        for function_index in range(rng.randint(12, 20)):
            parameters = ", ".join(f"arg_{rng.randrange(64)}" for _ in range(rng.randint(1, 5)))
            lines.append(f"def compute_{module_index}_{function_index}({parameters}):")
            for _ in range(rng.randint(4, 14)):
                kind = rng.random()
                first, second, third = (rng.randrange(64) for _ in range(3))
                if kind < 0.30:
                    chain = f" {rng.choice(operators)} ".join(
                        f"value_{rng.randrange(64)}" for _ in range(rng.randint(2, 5))
                    )
                    lines.append(f"    value_{first} = {chain}")
                elif kind < 0.50:
                    lines.append(f"    if value_{first} {rng.choice(comparisons)} value_{second}:")
                    lines.append(f"        value_{first} = value_{first} {rng.choice(operators)} value_{third}")
                elif kind < 0.65:
                    lines.append(f"    for step_{first} in range(value_{second} % 97):")
                    lines.append(f"        value_{third} += step_{first} {rng.choice(operators)} value_{rng.randrange(64)}")
                elif kind < 0.78:
                    lines.append(f"    while value_{first} > value_{second}:")
                    lines.append(f"        value_{first} -= 1")
                elif kind < 0.90:
                    lines.append(
                        f"    value_{first} = max(value_{second}, min(value_{third}, {rng.randint(0, 9999)}))"
                        f" + value_{rng.randrange(64)}"
                    )
                else:
                    arguments = ", ".join(f"value_{rng.randrange(64)}" for _ in range(rng.randint(1, 4)))
                    lines.append(f"    value_{first} = combine_{rng.randrange(64)}({arguments})")
            lines.append(f"    return value_{rng.randrange(64)} {rng.choice(operators)} value_{rng.randrange(64)}")
            lines.append("")
        last_text = "\n".join(lines) + "\n"
        (root / f"module_{module_index:04d}.py").write_text(last_text, encoding="utf-8")
        written += len(last_text.encode("utf-8"))
        module_index += 1
    return module_index, last_text


class HygieneTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.index = Path(self.temp.name) / "reference.spotidx.gz"
        self.file_count, self.token_count = hygiene.build_index(REFERENCE, self.index)

    def tearDown(self):
        self.temp.cleanup()

    def test_a_json_file_that_does_not_parse_yields_no_tokens(self):
        self.assertEqual(hygiene._json_tokens(""), [])
        self.assertEqual(hygiene._json_tokens("spot.journal.manifest.v1\0\0\x01"), [])

    def test_reference_corpus_has_ten_files_across_all_languages(self):
        self.assertEqual(self.file_count, 10)
        summary = hygiene.index_summary(self.index)
        self.assertEqual(len(summary["files"]), 10)
        suffixes = {Path(entry["path"]).suffix for entry in summary["files"]}
        self.assertEqual(suffixes, {".py", ".js", ".ts", ".rs", ".cs", ".json"})
        self.assertGreater(self.token_count, 300)
        self.assertGreater(summary["fingerprints"], 40)

    def test_seeded_copies_detected_and_negatives_measured(self):
        reports = hygiene.scan_targets(self.index, TARGETS)
        found = {report["target"] for report in reports}
        self.assertEqual(found, POSITIVE_TARGETS)
        detected = len(found & POSITIVE_TARGETS)
        precision = detected / len(found) if found else 1.0
        recall = detected / len(POSITIVE_TARGETS)
        print(
            f"synthetic corpus precision {precision:.2f} ({detected}/{len(found)} reports are true copies), "
            f"recall {recall:.2f} ({detected}/{len(POSITIVE_TARGETS)} seeded copies), "
            f"{len(INDEPENDENT_NEGATIVES)} independent negatives clean"
        )
        fragment = next(report for report in reports if report["target"].endswith("function_fragment.rs"))
        self.assertTrue(fragment["contiguous_regions"])
        self.assertGreaterEqual(fragment["contiguous_regions"][0]["tokens"], hygiene.LONG_MATCH)

    def test_independent_reimplementations_fingerprint_clean(self):
        # A "negative" too short to fingerprint would pass vacuously, so first
        # assert each independent reimplementation carries real fingerprints,
        # then assert the scanner does not report it.
        for relative in sorted(INDEPENDENT_NEGATIVES):
            source = (TARGETS / relative).read_text(encoding="utf-8")
            fingerprints = hygiene.fingerprints_for(relative, source)
            print(f"independent negative {relative}: {len(fingerprints)} winnowed fingerprints")
            self.assertGreaterEqual(len(fingerprints), 10, f"{relative} is too short to be a real negative")
        reports = hygiene.scan_targets(self.index, TARGETS)
        found = {report["target"] for report in reports}
        for relative in sorted(INDEPENDENT_NEGATIVES):
            self.assertNotIn(relative, found, f"independent reimplementation was flagged: {relative}")

    def test_javascript_template_interpolation_is_tokenized_as_code(self):
        original = hygiene.tokens_for("sample.js", "const label = `first ${measure(value + 2)}`;")
        renamed = hygiene.tokens_for("sample.js", "const caption=`second ${calculate(point + 99)}`;")
        original_values = [token.value for token in original]
        self.assertEqual(original_values, [token.value for token in renamed])
        self.assertIn("TEMPLATE_EXPR_START", original_values)
        self.assertIn("+", original_values)

    def test_csharp_interpolation_is_tokenized_as_code(self):
        original = hygiene.tokens_for("sample.cs", 'var label = $"value {measure(amount + 2)}";')
        renamed = hygiene.tokens_for("sample.cs", 'var caption = $@"other {calculate(point + 99)}";')
        values = [token.value for token in original]
        self.assertEqual(values, [token.value for token in renamed])
        self.assertIn("INTERPOLATION_START", values)
        self.assertIn("+", values)

    def test_allowlist_requires_a_reason_and_skips_only_matching_path(self):
        reports = hygiene.scan_targets(
            self.index, TARGETS, allow=[("python/renamed_copy.py", "invented fixture demonstrates allow-list behavior")]
        )
        found = {report["target"] for report in reports}
        self.assertNotIn("python/renamed_copy.py", found)
        self.assertEqual(found, POSITIVE_TARGETS - {"python/renamed_copy.py"})
        folder = hygiene.scan_targets(self.index, TARGETS, allow=[("typescript/*", "whole folder")])
        self.assertEqual(
            {report["target"] for report in folder},
            POSITIVE_TARGETS - {"typescript/renamed_ticket.ts"},
        )
        with self.assertRaisesRegex(ValueError, "path and reason"):
            hygiene.load_allowlist_data({"entries": [{"path": "somewhere.py"}]})

    @unittest.skipUnless(
        os.environ.get("SPOT_HYGIENE_BENCHMARK") == "1",
        "set SPOT_HYGIENE_BENCHMARK=1 to run the token-dense 50 MB synthetic index benchmark",
    )
    def test_index_50_mb_token_dense_source_within_three_minutes(self):
        try:
            import resource
        except ImportError:
            resource = None
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp) / "reference"
            root.mkdir()
            generated, probe_text = write_synthetic_python_corpus(root)
            index = Path(temp) / "dense.spotidx.gz"
            start = time.perf_counter()
            built_files, token_count = hygiene.build_index(root, index)
            elapsed = time.perf_counter() - start
            summary = hygiene.index_summary(index)
            size = index.stat().st_size
            peak = ""
            if resource is not None:
                peak = f", peak RSS {resource.getrusage(resource.RUSAGE_SELF).ru_maxrss / 1024:.0f} MiB"
            print(
                f"token-dense 50 MB index benchmark: {generated} generated files, {built_files} indexed, "
                f"{token_count} tokens, {summary['fingerprints']} fingerprints, "
                f"build {elapsed:.1f}s, index {size / 1e6:.1f} MB{peak}"
            )
            self.assertLess(elapsed, 180)
            probe = Path(temp) / "probe"
            probe.mkdir()
            renamed = probe_text.replace("value_", "val_").replace("    ", "  ")
            (probe / "sample.py").write_text(renamed, encoding="utf-8")
            scan_start = time.perf_counter()
            matches = hygiene.scan_targets(index, probe)
            scan_elapsed = time.perf_counter() - scan_start
            print(
                f"token-dense 50 MB scan benchmark: one {len(renamed) / 1e6:.2f} MB target "
                f"scanned in {scan_elapsed:.2f}s, {len(matches)} match(es)"
            )
            self.assertEqual(len(matches), 1)


if __name__ == "__main__":
    unittest.main()