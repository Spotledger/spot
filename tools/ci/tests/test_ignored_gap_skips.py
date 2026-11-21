import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import ignored_gap_skips as g  # noqa: E402

SRC = '''
#[tokio::test]
#[ignore = "gap: WP-X.T1 not built"]
async fn a_gap_test() {}

#[test]
#[ignore = "soak: needs hardware"]
fn a_nightly_test() {}

#[ignore = "gap: multi line \\
            reason"]
#[test]
fn another_gap() {}

// `#[ignore = "gap: in a comment"]`
fn not_a_gap() {}
'''


class GapSkips(unittest.TestCase):
    def test_only_gap_tests_are_named(self):
        self.assertEqual(g.gap_tests(SRC), ["a_gap_test", "another_gap"])

    def test_collect_walks_roots_and_skips_target(self):
        with tempfile.TemporaryDirectory() as d:
            (Path(d) / "t").mkdir()
            (Path(d) / "t" / "x.rs").write_text(SRC)
            (Path(d) / "target").mkdir()
            (Path(d) / "target" / "y.rs").write_text('#[ignore = "gap: z"]\nfn hidden() {}\n')
            self.assertEqual(g.collect([d]), ["a_gap_test", "another_gap"])

    def test_the_repository_has_gap_tests_and_the_failing_one_is_named(self):
        root = Path(__file__).resolve().parents[3]
        names = g.collect([root / "crates", root / "harness"])
        self.assertIn("the_publish_refusal_for_an_unenforceable_schema_names_the_node_and_the_code", names)


if __name__ == "__main__":
    unittest.main()
