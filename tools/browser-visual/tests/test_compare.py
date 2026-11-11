"""The browser comparator: identical, inside tolerance, outside tolerance, size, missing (WP-UI-16.T2)."""
from __future__ import annotations

import contextlib
import io
import tempfile
import unittest
from pathlib import Path
import shutil

import _support
from _support import compare, png

TOLERANCE = compare.Tolerance()


class ComparisonTest(unittest.TestCase):
    def setUp(self):
        self.base = png.solid(4, 3, (10, 20, 30, 255))

    def test_identical_images_match(self):
        result = compare.compare('identical', self.base, self.base, TOLERANCE)
        self.assertTrue(result.ok)
        self.assertEqual(result.status, 'match')
        self.assertEqual(result.changed, 0)
        self.assertIn('match', result.describe(TOLERANCE))

    def test_one_pixel_inside_the_channel_delta_matches(self):
        nudged = png.paint(self.base, 1, 1, (12, 20, 30, 255))  # exactly the delta of 2
        result = compare.compare('nudged', self.base, nudged, TOLERANCE)
        self.assertTrue(result.ok, result.describe(TOLERANCE))
        self.assertEqual(result.changed, 0)

    def test_one_pixel_outside_the_delta_but_inside_the_fraction_matches(self):
        # 12 pixels total, one changed by 3: 8.3% > 0.1%, so this must fail at the default.
        loud = png.paint(self.base, 1, 1, (13, 20, 30, 255))
        self.assertFalse(compare.compare('loud', self.base, loud, TOLERANCE).ok)
        # With a reviewed, wider fraction the same pair is allowed: tolerance is explicit data.
        relaxed = compare.Tolerance(channel_delta=2, changed_pixel_fraction=0.1)
        result = compare.compare('relaxed', self.base, loud, relaxed)
        self.assertTrue(result.ok, result.describe(relaxed))
        self.assertEqual(result.changed, 1)

    def test_changed_pixels_are_counted_per_pixel_not_per_channel(self):
        twice = png.paint(png.paint(self.base, 0, 0, (99, 20, 30, 255)), 1, 0, (99, 20, 30, 255))
        result = compare.compare('twice', self.base, twice, TOLERANCE)
        self.assertEqual(result.changed, 2)
        self.assertEqual(result.ratio, 2 / 12)

    def test_a_size_mismatch_fails_regardless_of_tolerance(self):
        wider = png.solid(5, 3, (10, 20, 30, 255))
        result = compare.compare('wider', self.base, wider, compare.Tolerance(255, 1.0))
        self.assertFalse(result.ok)
        self.assertEqual(result.status, 'size')
        self.assertIn('SIZE MISMATCH', result.describe())

    def test_a_missing_baseline_fails_with_the_update_command(self):
        with tempfile.TemporaryDirectory() as directory:
            capture = Path(directory) / 'login-1280x800.png'
            png.write(capture, self.base)
            result, actual, expected = compare.compare_files('login-1280x800', capture,
                                                             Path(directory) / 'absent.png', TOLERANCE)
            self.assertFalse(result.ok)
            self.assertEqual(result.status, 'missing')
            self.assertIsNone(expected)
            self.assertEqual(actual.pixel(0, 0), bytes((10, 20, 30, 255)))
            self.assertIn('run.sh --update', result.describe())

    def test_tolerance_is_validated(self):
        for delta, fraction in ((-1, 0.1), (256, 0.1), (2, -0.1), (2, 1.1)):
            with self.assertRaises(compare.CompareError, msg=f'delta {delta}, fraction {fraction}'):
                compare.Tolerance(delta, fraction)

    def test_the_diff_marks_changed_red_and_absent_magenta(self):
        actual = png.solid(6, 3, (10, 20, 30, 255))
        actual = png.paint(actual, 0, 0, (250, 250, 250, 255))
        difference = compare.diff(self.base, actual, TOLERANCE)
        self.assertEqual((difference.width, difference.height), (6, 3))
        self.assertEqual(difference.pixel(0, 0), compare.CHANGED_COLOUR)
        self.assertEqual(difference.pixel(1, 1), compare.UNCHANGED_COLOUR)
        self.assertEqual(difference.pixel(5, 0), compare.ABSENT_COLOUR)

    def test_wrong_sized_buffers_are_rejected_by_the_png_writer(self):
        broken = png.Image(2, 2, b'\x00' * 4)
        with self.assertRaises(png.PngError):
            png.encode(broken)


class RunTest(unittest.TestCase):
    def setUp(self):
        self.root = Path(tempfile.mkdtemp(prefix='browser-visual-'))
        self.addCleanup(shutil.rmtree, self.root, True)
        self.captures = self.root / 'captures'
        self.baselines = self.root / 'baselines'
        self.out = self.root / 'out'
        self.captures.mkdir()
        self.baselines.mkdir()
        png.write(self.captures / 'login-1280x800.png', png.solid(4, 3, (10, 20, 30, 255)))
        png.write(self.baselines / 'login-1280x800.png', png.solid(4, 3, (10, 20, 30, 255)))

    def test_a_clean_run_writes_no_review_artefacts_and_passes(self):
        comparisons, failures = compare.run(self.captures, self.baselines, self.out, TOLERANCE)
        self.assertEqual(failures, [])
        self.assertEqual(len(comparisons), 1)
        self.assertFalse((self.out / 'review').exists())

    def test_a_mismatch_writes_three_images_and_a_review_page(self):
        png.write(self.captures / 'login-1280x800.png', png.solid(4, 3, (200, 20, 30, 255)))
        comparisons, failures = compare.run(self.captures, self.baselines, self.out, TOLERANCE)
        self.assertEqual(len(failures), 1)
        self.assertEqual(comparisons[0].status, 'pixels')
        review = self.out / 'review'
        for kind in ('expected', 'actual', 'diff'):
            self.assertTrue((review / f'login-1280x800-{kind}.png').is_file(), kind)
        page = (review / 'login-1280x800.html').read_text(encoding='utf-8')
        self.assertIn('login-1280x800', page)
        self.assertIn('run.sh --update', page)
        self.assertIn('<img alt="diff"', page)

    def test_update_writes_the_baseline_and_then_passes(self):
        png.write(self.captures / 'login-1280x800.png', png.solid(4, 3, (1, 2, 3, 255)))
        comparisons, failures = compare.run(self.captures, self.baselines, self.out, TOLERANCE, update=True)
        self.assertEqual(failures, [])
        self.assertEqual(comparisons[0].status, 'match')
        self.assertEqual(png.read(self.baselines / 'login-1280x800.png').pixel(0, 0), bytes((1, 2, 3, 255)))
        self.assertFalse((self.out / 'review').exists())

    def test_an_empty_capture_directory_is_an_error_not_a_pass(self):
        with self.assertRaises(compare.CompareError):
            compare.run(self.root / 'missing', self.baselines, self.out, TOLERANCE)
        empty = self.root / 'empty'
        empty.mkdir()
        with self.assertRaises(compare.CompareError):
            compare.run(empty, self.baselines, self.out, TOLERANCE)

    def test_the_cli_reports_failures_in_its_exit_code(self):
        out, err = io.StringIO(), io.StringIO()
        with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
            code = compare.main(['--captures', str(self.captures), '--baselines', str(self.baselines),
                                 '--out', str(self.out)])
            self.assertEqual(code, 0)
            png.write(self.captures / 'login-1280x800.png', png.solid(4, 3, (9, 9, 9, 255)))
            self.assertEqual(compare.main(['--captures', str(self.captures), '--baselines', str(self.baselines),
                                           '--out', str(self.out)]), 1)
        self.assertIn('1 case(s) compared', out.getvalue())
        self.assertIn('review:', err.getvalue())


if __name__ == '__main__':
    unittest.main()
