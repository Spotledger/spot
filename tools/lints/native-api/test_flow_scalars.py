import unittest
from pathlib import Path

import flow_scalars as fs

GOOD = """
a: {type: string, enum: [a, b]}
b: {type: string, description: "Total digits, for decimals."}
c: {type: string, description: 'Quoted, with comma.'}
d:
  description: |
    Block scalar, with commas. And more.
  x: {type: integer, description: Plain without comma.}
e: {type: array, items: {type: string}, example: [one, two]}
"""


class FlowScalars(unittest.TestCase):
    def test_catches_decimals_defect(self):
        v = fs.find_violations("p: {type: integer, description: Total digits, for decimals.}\n")
        self.assertEqual(v, [(1, "for decimals.")])

    def test_catches_etag_defect_in_nested_flow(self):
        text = "x:\n  y: {type: string, description: The version, in ETag form. It changes when rules change.}\n"
        v = fs.find_violations(text)
        self.assertEqual([k for _, k in v], ["in ETag form. It changes when rules change."])
        self.assertEqual(v[0][0], 2)

    def test_catches_split_inside_flow_sequence(self):
        v = fs.find_violations("l: [{description: One, and two.}]\n")
        self.assertEqual([k for _, k in v], ["and two."])

    def test_clean_input_not_flagged(self):
        self.assertEqual(fs.find_violations(GOOD), [])

    def test_block_mapping_null_key_not_flagged(self):
        self.assertEqual(fs.find_violations("a:\n  some key text:\n"), [])

    def test_allow_list_suppresses_and_reports_stale(self):
        p = Path(__file__).with_name("_tmp_flow.yaml")
        try:
            p.write_text("p: {description: Total digits, for decimals.}\n", encoding="utf-8")
            self.assertEqual(fs.check(p, allow={"for decimals."}), [])
            self.assertEqual(len(fs.check(p, allow=set())), 1)
            self.assertIn("no longer needed", fs.check(p, allow={"for decimals.", "gone."})[0])
        finally:
            p.unlink(missing_ok=True)

    def test_real_file_has_exactly_the_known_defects(self):
        text = fs.DEFAULT_PATH.read_text(encoding="utf-8")
        keys = {k for _, k in fs.find_violations(text)}
        if keys < fs.KNOWN_BAD:
            self.fail("the allow-list entry is no longer needed, delete it: " + repr(sorted(fs.KNOWN_BAD - keys)))
        self.assertEqual(keys, fs.KNOWN_BAD)
        self.assertEqual(fs.check(fs.DEFAULT_PATH), [])


if __name__ == "__main__":
    unittest.main()
