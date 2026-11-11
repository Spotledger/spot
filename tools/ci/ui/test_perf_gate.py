"""Fail-closed contract tests; fixtures are measurements, not CI evidence."""
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

sys.path.insert(0, str(Path(__file__).parent))
import perf_gate

DECLARATION = '''[[budget]]
id = "paint"
metric = "p95"
max = 10
unit = "ms"
runner = "reference"
'''


class GateTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.budget = self.root / 'perf-budgets.toml'
        self.budget.write_text(DECLARATION)
        self.results = self.root / 'results'
        self.results.mkdir()

    def write_result(self, measured=11, **changes):
        record = dict(id='paint', measured=measured, max=10, runner='ci-linux', commit='abc', metric='p95', unit='ms')
        record.update(changes)
        (self.results / 'result.json').write_text(json.dumps(record))

    def evaluate(self, kind='hosted'):
        return perf_gate.evaluate([self.budget], self.results, 'ci-linux', kind, 'abc')

    def test_breach_and_missing_are_failures(self):
        missing = self.evaluate()
        self.assertFalse(missing.ok)
        self.assertIn('MISSING', missing.markdown)
        self.write_result()
        breach = self.evaluate()
        self.assertFalse(breach.ok)
        self.assertIn('FAIL', breach.markdown)
        self.assertIn('APPLIED MULTIPLIER', breach.markdown)
        self.write_result(measured=10)
        self.assertTrue(self.evaluate().ok)


    def test_runner_table_does_not_relax_hosted_enforcement(self):
        self.budget.write_text(DECLARATION + '\n[runner.ci-linux]\nmultiplier = 4\n')
        self.write_result(measured=100)
        self.assertFalse(self.evaluate('hosted').ok)
        warning = self.evaluate('self-hosted')
        self.assertTrue(warning.ok)
        self.assertIn('WARNING', warning.markdown)
        self.assertIn('**4x**', warning.markdown)

    def test_advisory_turns_breaches_into_warnings_but_not_missing_results(self):
        self.write_result(measured=500)
        self.assertFalse(perf_gate.evaluate([self.budget], self.results, 'ci-linux', 'hosted', 'abc').ok)
        advisory = perf_gate.evaluate([self.budget], self.results, 'ci-linux', 'hosted', 'abc', advisory=True)
        self.assertTrue(advisory.ok)
        self.assertIn('WARNING', advisory.markdown)
        self.assertIn('ADVISORY', advisory.markdown)
        self.assertIn('**1x**', advisory.markdown)
        (self.results / 'result.json').unlink()
        self.assertFalse(perf_gate.evaluate([self.budget], self.results, 'ci-linux', 'hosted', 'abc', advisory=True).ok)

    def test_shared_default_is_loud_and_missing_still_fails(self):
        self.write_result(measured=31)
        result = self.evaluate('self-hosted')
        self.assertTrue(result.ok)
        self.assertIn('**3x**', result.markdown)
        self.assertIn('WARNING', result.markdown)
        (self.results / 'result.json').unlink()
        self.assertFalse(self.evaluate('self-hosted').ok)

    def test_unknown_id_is_rejected(self):
        self.write_result(id='unknown')
        with self.assertRaisesRegex(perf_gate.GateError, 'unknown'):
            self.evaluate()

    def test_malformed_files_are_rejected(self):
        for value in ('{', '[]', '{}', 'null', '{"id":"paint","id":"paint"}'):
            with self.subTest(value=value):
                (self.results / 'bad.json').write_text(value)
                with self.assertRaises(perf_gate.GateError):
                    self.evaluate()

    def test_invalid_measurements_are_rejected(self):
        for value in (-1, float('nan'), float('inf'), True, '10', None):
            with self.subTest(value=value):
                self.write_result(measured=value)
                with self.assertRaises(perf_gate.GateError):
                    self.evaluate()

    def test_stale_wrong_os_and_changed_contract_are_rejected(self):
        for field, value in [('commit', 'stale'), ('runner', 'ci-windows'), ('max', 100), ('max', True), ('metric', 'max'), ('unit', 'seconds')]:
            with self.subTest(field=field):
                self.write_result(**{field: value})
                with self.assertRaises(perf_gate.GateError):
                    self.evaluate()

    def test_later_pass_cannot_mask_an_earlier_breach(self):
        self.write_result(measured=20)
        (self.results / 'result.json').rename(self.results / 'old.json')
        self.write_result(measured=1)
        self.assertFalse(self.evaluate().ok)

    def test_malformed_budget_is_not_silently_ignored(self):
        for content in ('', 'oops = 1', '[[budget]]', DECLARATION.replace('max = 10', 'max = nan'), DECLARATION + '\n[runner.reference]\nmultiplier = 3'):
            with self.subTest(content=content):
                self.budget.write_text(content)
                with self.assertRaises(perf_gate.GateError):
                    self.evaluate()

    def test_duplicate_budget_files_fail(self):
        with self.assertRaisesRegex(perf_gate.GateError, 'Duplicate'):
            perf_gate.load_budgets([self.budget, self.budget])

    def test_legacy_grid_keys_have_exact_stable_ids(self):
        self.budget.write_text('max_frame_ms = 8.0\nmax_alloc_bytes_per_frame = 4096\nfirst_paint_50_rows_ms = 50.0\nrows = 100000\ncolumns = 12\nscroll_steps = 300\n')
        entries = perf_gate.load_budgets([self.budget])
        self.assertEqual(set(entries), {'grid.frame.max', 'grid.frame.alloc.mean', 'grid.first-paint'})
        self.assertEqual(entries['grid.frame.alloc.mean'].max, 4096)
        self.assertFalse(self.evaluate().ok)
        self.assertEqual(self.evaluate().markdown.count('MISSING'), 3)

    def test_windows_only_budget_is_not_required_on_linux(self):
        self.budget.write_text(DECLARATION + '\n[[budget]]\nid = "win"\nmetric = "max"\nmax = 20\nunit = "ms"\nrunner = "ci-windows"\n')
        self.write_result(measured=1)
        self.assertTrue(self.evaluate().ok)

    def test_budget_discovery_excludes_build_output_and_fails_when_empty(self):
        source = self.root / 'tests'
        (source / 'One').mkdir(parents=True)
        (source / 'One' / 'perf-budgets.toml').write_text(DECLARATION)
        (source / 'One' / 'bin' / 'Debug').mkdir(parents=True)
        (source / 'One' / 'bin' / 'Debug' / 'perf-budgets.toml').write_text(DECLARATION)
        self.assertEqual(perf_gate.discover_budgets(source), [source / 'One' / 'perf-budgets.toml'])
        with self.assertRaises(perf_gate.GateError):
            perf_gate.discover_budgets(self.results)

    def test_cli_writes_read_only_step_summary_and_fails_closed(self):
        summary = self.root / 'summary.md'
        command = [sys.executable, str(Path(perf_gate.__file__)), '--budget', str(self.budget), '--results', str(self.results), '--runner', 'ci-linux', '--kind', 'hosted', '--commit', 'abc']
        self.write_result(measured=2)
        run = subprocess.run(command, env={**os.environ, 'GITHUB_STEP_SUMMARY': str(summary)}, capture_output=True, text=True)
        self.assertEqual(run.returncode, 0, run.stderr)
        self.assertIn('PASS', summary.read_text())
        (self.results / 'bad.json').write_text('{')
        run = subprocess.run(command, env={**os.environ, 'GITHUB_STEP_SUMMARY': str(summary)}, capture_output=True, text=True)
        self.assertEqual(run.returncode, 1)
        self.assertIn('ERROR', summary.read_text())


if __name__ == '__main__':
    unittest.main()
