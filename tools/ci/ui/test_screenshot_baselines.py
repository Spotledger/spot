"""Tests for the per-platform screenshot baseline inventory (WP-UI-16.T2)."""
from __future__ import annotations

import contextlib
import io
import json
from pathlib import Path
import shutil
import tempfile
import unittest

import screenshot_baselines as gate

CASE_A = 'matrix-64x48-comfortable-light-ltr'
CASE_B = 'grid-selected-960x480-compact-dark-rtl'


class InventoryTest(unittest.TestCase):
    def setUp(self):
        self.root = Path(tempfile.mkdtemp(prefix='screenshot-baselines-'))
        self.addCleanup(shutil.rmtree, self.root, True)

    def write(self, platform, case):
        directory = self.root / platform
        directory.mkdir(parents=True, exist_ok=True)
        (directory / f'{case}.png').write_bytes(b'\x89PNG\r\n\x1a\n')

    def exceptions(self, entries):
        (self.root / gate.EXCEPTIONS).write_text(json.dumps({'exceptions': entries}), encoding='utf-8')

    def test_a_complete_platform_passes_and_counts_its_cases(self):
        for platform in gate.PLATFORMS:
            self.write(platform, CASE_A)
            self.write(platform, CASE_B)
        inventory = gate.load(self.root)
        self.assertEqual(gate.verify(inventory, 'linux'), 2)
        self.assertEqual(inventory.missing('windows'), ())

    def test_a_missing_baseline_fails_and_names_the_update_command(self):
        self.write('linux', CASE_A)
        self.write('linux', CASE_B)
        self.write('windows', CASE_A)
        inventory = gate.load(self.root)
        self.assertEqual(inventory.missing('windows'), (CASE_B,))
        with self.assertRaises(gate.BaselineError) as caught:
            gate.verify(inventory, 'windows')
        message = str(caught.exception)
        self.assertIn('1 baseline(s) missing for windows', message)
        self.assertIn(CASE_B, message)
        self.assertIn('UPDATE_BASELINES=1 dotnet test', message)
        self.assertIn('Never copy another platform', message)

    def test_an_empty_platform_is_never_treated_as_no_expectations(self):
        self.write('linux', CASE_A)
        inventory = gate.load(self.root)
        self.assertEqual(inventory.missing('windows'), (CASE_A,))

    def test_an_approved_single_platform_exception_needs_a_reason(self):
        self.write('linux', CASE_A)
        self.write('windows', CASE_A)
        self.exceptions([{'platform': 'linux', 'case': CASE_A, 'reason': 'linux-only font stack'}])
        inventory = gate.load(self.root)
        self.assertEqual(inventory.missing('linux'), ())
        self.assertEqual(inventory.missing('windows'), ())
        self.exceptions([{'platform': 'linux', 'case': CASE_A, 'reason': '   '}])
        with self.assertRaises(gate.BaselineError):
            gate.load(self.root)

    def test_an_unknown_exception_platform_is_rejected(self):
        self.write('linux', CASE_A)
        self.exceptions([{'platform': 'solaris', 'case': CASE_A, 'reason': 'no'}])
        with self.assertRaises(gate.BaselineError):
            gate.load(self.root)

    def test_a_stray_file_name_is_rejected(self):
        self.write('linux', CASE_A)
        (self.root / 'linux' / 'copy-of-mine.png').write_bytes(b'\x89PNG\r\n\x1a\n')
        with self.assertRaises(gate.BaselineError) as caught:
            gate.load(self.root)
        self.assertIn('is not <case>-<w>x<h>-<density>-<theme>-<direction>.png', str(caught.exception))

    def test_a_non_png_is_rejected(self):
        self.write('linux', CASE_A)
        (self.root / 'linux' / 'notes.txt').write_text('todo', encoding='utf-8')
        with self.assertRaises(gate.BaselineError):
            gate.load(self.root)

    def test_no_baselines_at_all_is_an_error_not_a_pass(self):
        with self.assertRaises(gate.BaselineError) as caught:
            gate.load(self.root)
        self.assertIn('no baselines at all', str(caught.exception))

    def test_a_missing_root_is_an_error(self):
        with self.assertRaises(gate.BaselineError):
            gate.load(self.root / 'absent')

    def test_unknown_platform_is_rejected(self):
        self.write('linux', CASE_A)
        inventory = gate.load(self.root)
        with self.assertRaises(gate.BaselineError):
            inventory.missing('beos')

    def test_cli_check_list_and_generate_command(self):
        self.write('linux', CASE_A)
        self.write('linux', CASE_B)
        for platform in ('windows', 'macos'):
            self.write(platform, CASE_A)
            self.write(platform, CASE_B)
        out, err = io.StringIO(), io.StringIO()
        with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
            self.assertEqual(gate.main(['--platform', 'linux', '--root', str(self.root), '--check']), 0)
            self.assertEqual(gate.main(['--platform', 'linux', '--root', str(self.root), '--list']), 0)
            self.assertEqual(gate.main(['--platform', 'windows', '--generate-command']), 0)
        self.assertIn('has all 2 committed cases', out.getvalue())
        self.assertIn(CASE_B, out.getvalue())
        self.assertIn('UPDATE_BASELINES=1 dotnet test', out.getvalue())
        self.assertEqual(err.getvalue(), '')

    def test_cli_failure_exits_non_zero(self):
        self.write('linux', CASE_A)
        err = io.StringIO()
        with contextlib.redirect_stderr(err):
            self.assertEqual(gate.main(['--platform', 'windows', '--root', str(self.root), '--check']), 1)
        self.assertIn('missing for windows', err.getvalue())

    def test_the_committed_repository_inventory_passes_on_this_platform(self):
        """The real Baselines/ tree must be complete for the platform running the tests."""
        repository = Path(__file__).resolve().parents[3]
        root = repository / gate.DEFAULT_ROOT
        platform = 'windows' if __import__('os').name == 'nt' else (
            'macos' if __import__('sys').platform == 'darwin' else 'linux')
        inventory = gate.load(root)
        self.assertGreaterEqual(gate.verify(inventory, platform), 8)


if __name__ == '__main__':
    unittest.main()
