import contextlib
import io
import json
import os
import shutil
import subprocess
import sys
import tempfile
import textwrap
import unittest
from pathlib import Path

HYGIENE_DIR = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(HYGIENE_DIR))
import hygiene
import reference


def _find_posix_bash() -> list[str] | None:
    """A bash that actually runs POSIX shell scripts.

    On Windows the System32 bash.exe on PATH is the WSL launcher, which fails
    with exit code 1 when no WSL distribution is installed; Git for Windows'
    bash.exe is the one that executes .sh scripts. Returns None only when no
    bash exists at all.
    """
    if os.name != "nt":
        found = shutil.which("bash")
        return [found] if found else None
    program_dirs = (
        os.environ.get("ProgramFiles", "C:\\Program Files"),
        os.environ.get("ProgramFiles(x86)", "C:\\Program Files (x86)"),
    )
    for program_dir in program_dirs:
        candidate = Path(program_dir) / "Git" / "bin" / "bash.exe"
        if candidate.is_file():
            return [str(candidate)]
    return None


def fake_entry(repo: str = "frappe/frappe") -> reference.PinEntry:
    return reference.PinEntry(
        repo=repo,
        tag="v0.0.0",
        commit="a" * 40,
        url=f"https://github.com/{repo}/archive/{'a' * 40}.tar.gz",
        sha256="b" * 64,
    )


def write_pin(tmp: Path, **overrides) -> Path:
    entry = {
        "repo": "frappe/frappe",
        "tag": "v16.35.0",
        "commit": "a" * 40,
        "url": f"https://github.com/frappe/frappe/archive/{'a' * 40}.tar.gz",
        "sha256": "b" * 64,
    }
    entry.update(overrides)
    body = "".join(f'{key} = "{value}"\n' for key, value in entry.items())
    path = tmp / "reference.toml"
    path.write_text(f"[[references]]\n{body}", encoding="utf-8")
    return path


class LoadPinValidation(unittest.TestCase):
    def test_valid_pin_loads(self):
        with tempfile.TemporaryDirectory() as name:
            entries = reference.load_pin(write_pin(Path(name)))
            self.assertEqual(len(entries), 1)
            self.assertEqual(entries[0].tag, "v16.35.0")
            self.assertEqual(entries[0].slug, "frappe-frappe")
            self.assertEqual(entries[0].prefix, "aaaaaaaa")

    def test_short_commit_rejected(self):
        with tempfile.TemporaryDirectory() as name:
            with self.assertRaises(reference.ReferenceError):
                reference.load_pin(write_pin(Path(name), commit="a" * 39))

    def test_short_digest_rejected(self):
        with tempfile.TemporaryDirectory() as name:
            with self.assertRaises(reference.ReferenceError):
                reference.load_pin(write_pin(Path(name), sha256="b" * 63))

    def test_plain_http_url_rejected(self):
        with tempfile.TemporaryDirectory() as name:
            url = f"http://github.com/frappe/frappe/archive/{'a' * 40}.tar.gz"
            with self.assertRaises(reference.ReferenceError):
                reference.load_pin(write_pin(Path(name), url=url))

    def test_url_must_embed_the_commit(self):
        with tempfile.TemporaryDirectory() as name:
            url = "https://github.com/frappe/frappe/archive/v16.35.0.tar.gz"
            with self.assertRaises(reference.ReferenceError):
                reference.load_pin(write_pin(Path(name), url=url))

    def test_empty_digest_only_allowed_for_bootstrap(self):
        with tempfile.TemporaryDirectory() as name:
            path = write_pin(Path(name), sha256="")
            with self.assertRaises(reference.ReferenceError):
                reference.load_pin(path)
            entries = reference.load_pin(path, require_digest=False)
            self.assertEqual(len(entries), 1)


class ProgrammaticTransforms(unittest.TestCase):
    PYTHON_SOURCE = textwrap.dedent(
        """\
        def compute_total(rows, tax_rate):
            total = 0
            for row in rows:
                if row.active:
                    total += row.amount * row.count
            return total + total * tax_rate


        class InvoiceBasket:
            def render(self, rows):
                return compute_total(rows, 0.2)
        """
    )

    def test_rename_changes_identifiers_keeps_keywords(self):
        renamed = reference._rename_transform(self.PYTHON_SOURCE, "python")
        self.assertNotIn("compute_total", renamed)
        self.assertNotIn("InvoiceBasket", renamed)
        for keyword in ("def", "class", "return", "for", "if"):
            self.assertIn(keyword, renamed)
        # whitespace-only reformat must keep every statement line
        self.assertEqual(
            len([line for line in renamed.splitlines() if line.strip()]),
            len([line for line in self.PYTHON_SOURCE.splitlines() if line.strip()]),
        )

    def test_json_transform_keeps_structure_changes_values(self):
        original = {"a": 1, "b": [True, "x", 1.5], "c": {"d": "y"}, "e": None}
        changed = json.loads(reference._json_values_transform(json.dumps(original)))
        self.assertEqual(list(changed), ["a", "b", "c", "e"])
        self.assertEqual(list(changed["c"]), ["d"])
        self.assertIsNone(changed["e"])
        self.assertNotEqual(changed["a"], original["a"])
        self.assertNotEqual(changed["b"][2], original["b"][2])

    def test_fragment_moves_source_lines_into_a_wrapper(self):
        lines = [f"probe_value_{number} = {number} + {number}" for number in range(80)]
        lines += ["def finish():", "    return 1"]
        source = "\n".join(lines)
        fragment = reference._fragment_transform(source, ".py")
        self.assertIn("def unrelated_wrapper():", fragment)
        body = [line for line in fragment.splitlines() if "probe_value_" in line]
        self.assertTrue(body)
        self.assertTrue(all(line.strip() in source for line in body))


class SeededSelection(unittest.TestCase):
    def _corpus(self, root: Path) -> Path:
        corpus = root / "corpus"
        (corpus / "python").mkdir(parents=True)
        (corpus / "javascript").mkdir()
        (corpus / "json").mkdir()
        for number in range(12):
            body = "\n".join(f"value_{number}_{line} = {line} + 1" for line in range(100))
            (corpus / "python" / f"module_{number}.py").write_text(body, encoding="utf-8")
            body = "\n".join(f"const grid_{number}_{line} = {line};" for line in range(100))
            (corpus / "javascript" / f"grid_{number}.js").write_text(body, encoding="utf-8")
            (corpus / "json" / f"doctype_{number}.json").write_text(
                json.dumps({"name": f"Doctype{number}", "fields": [{"i": line} for line in range(40)]}),
                encoding="utf-8",
            )
        return corpus

    def test_selection_is_deterministic_and_mixed(self):
        with tempfile.TemporaryDirectory() as name:
            corpus = self._corpus(Path(name))
            seed = reference.seed_from_entries([fake_entry()])
            first = reference.select_reference_files(corpus, 6, seed)
            second = reference.select_reference_files(corpus, 6, seed)
            self.assertEqual([rel for _, rel, _ in first], [rel for _, rel, _ in second])
            buckets = {bucket for _, _, bucket in first}
            self.assertEqual(buckets, {"python", "javascript", reference.JSON_BUCKET})


class SelectionRejectsShortCodeFiles(unittest.TestCase):
    def test_code_files_too_short_for_a_fragment_are_not_selected(self):
        with tempfile.TemporaryDirectory() as name:
            root = Path(name)
            corpus = root / "corpus"
            (corpus / "python").mkdir(parents=True)
            (corpus / "javascript").mkdir()
            (corpus / "json").mkdir()
            for number in range(4):
                body = "\n".join(f"short_value_{number}_{line} = {line}" for line in range(45))
                (corpus / "python" / f"short_{number}.py").write_text(body, encoding="utf-8")
            body = "\n".join(f"const tiny_{line} = {line};" for line in range(45))
            (corpus / "javascript" / "tiny.js").write_text(body, encoding="utf-8")
            for number in range(3):
                (corpus / "json" / f"doctype_{number}.json").write_text(
                    json.dumps({"name": f"Doctype{number}", "fields": [{"i": line} for line in range(40)]}),
                    encoding="utf-8",
                )
            seed = reference.seed_from_entries([fake_entry()])
            chosen = reference.select_reference_files(corpus, 6, seed)
            self.assertTrue(chosen)
            self.assertEqual(
                {bucket for _, _, bucket in chosen}, {reference.JSON_BUCKET}
            )


class ScanOutputHidesReferenceSource(unittest.TestCase):
    """ADR-32 guard: the scanner prints paths and line numbers, never source."""

    MARKER = "SPOT-HYGIENE-UNIQUE-MARKER-7f3a9c"

    # A small realistic module: the shapes vary enough that the fingerprints
    # survive the low-information filter (WP-TST-13.T3); the whole point of
    # this fixture is that a matching copy prints paths and line numbers, and
    # the unique marker must never leak into stdout or stderr.
    MODULE = '''\
MARKER = "SPOT-HYGIENE-UNIQUE-MARKER-7f3a9c"


def settle_batch(batch, ledger, rate):
    """Post entries to the ledger and return a small report."""
    total = 0
    flagged = []
    report = {}
    for position, entry in enumerate(batch):
        amount = entry.amount * (1 + rate)
        if amount < MIN_TRANSFER:
            raise ValueError("amount below floor", position)
        if amount > ledger.limit:
            amount = ledger.limit
            flagged.append(position)
        total += amount
        ledger.post(entry.account, amount)
    report["total"] = round(total, 2)
    report["flagged"] = flagged
    report["count"] = len(batch)
    ledger.flush(report)
    return report


def reconcile(statements, expectations, ledger):
    """Compare posted statements with the planned expectations."""
    differences = []
    for index, statement in enumerate(statements):
        expected = expectations[index]
        actual = statement.balance
        if abs(actual - expected) > TOLERANCE:
            differences.append((index, actual, expected))
            ledger.note(statement, "drift")
    return differences


class PlanBuilder:
    """Turn raw rows into scheduled ledger plan entries."""

    def __init__(self, rate, limit):
        self.rate = rate
        self.limit = limit

    def entries(self, rows):
        plan = []
        for row in rows:
            amount = row.amount * self.rate
            if amount >= self.limit:
                plan.append((row.account, amount, "high"))
            else:
                plan.append((row.account, amount, "low"))
        return plan


def summarise(plan):
    """Count high and low scheduled entries for the operator report."""
    highs = sum(1 for entry in plan if entry[2] == "high")
    totals = {"high": highs, "low": len(plan) - highs}
    return totals
'''

    def test_marker_never_reaches_stdout_or_stderr(self):
        with tempfile.TemporaryDirectory() as name:
            root = Path(name)
            reference_dir = root / "reference"
            target_dir = root / "targets"
            reference_dir.mkdir()
            target_dir.mkdir()
            body = self.MODULE
            (reference_dir / "opaque_reference.py").write_text(body, encoding="utf-8")
            (target_dir / "opaque_copy.py").write_text(body, encoding="utf-8")
            index = root / "index.bin"
            hygiene.build_index(reference_dir, index)
            result = subprocess.run(
                [sys.executable, str(HYGIENE_DIR / "hygiene.py"), "--index", str(index), "--target", str(target_dir)],
                capture_output=True,
                text=True,
                timeout=120,
            )
            self.assertIn("MATCH", result.stdout)
            self.assertNotIn(self.MARKER, result.stdout)
            self.assertNotIn(self.MARKER, result.stderr)

    def test_run_sh_is_inert_without_an_index(self):
        bash = _find_posix_bash()
        if bash is None:
            self.skipTest("no POSIX-capable bash on this machine")
        script = str(HYGIENE_DIR / "run.sh").replace("\\", "/")
        result = subprocess.run(
            bash + [script],
            capture_output=True,
            text=True,
            timeout=60,
        )
        self.assertEqual(result.returncode, 0)
        self.assertIn("no reference index configured", result.stdout)


class MiniRecallEndToEnd(unittest.TestCase):
    def test_renamed_copy_cites_its_origin(self):
        with tempfile.TemporaryDirectory() as name:
            root = Path(name)
            reference_dir = root / "reference"
            target_dir = root / "targets"
            reference_dir.mkdir()
            target_dir.mkdir()
            # The synthetic copy needs fingerprint-bearing shapes, not a long
            # run of one assignment shape: since WP-TST-13.T3 the low-
            # information filter drops literal-only and period-dominated
            # windows, so a 200-line run of `a = n + 7` yields no fingerprints
            # on either side. This fixture exercises the same recall property
            # (a renamed, reformatted copy is traced to its origin file) with
            # statement shapes that survive the filter.
            body = ScanOutputHidesReferenceSource.MODULE
            (reference_dir / "ledger.py").write_text(body, encoding="utf-8")
            index = root / "index.bin"
            hygiene.build_index(reference_dir, index)
            renamed = reference._rename_transform(body, "python")
            (target_dir / "renamed_ledger.py").write_text(renamed, encoding="utf-8")
            reports = hygiene.scan_targets(index, target_dir, threshold=0.30)
            self.assertTrue(reports)
            cited = {ref["path"] for report in reports for ref in report["references"]}
            self.assertIn("ledger.py", cited)
class PrintIndexPathColdCacheSafe(unittest.TestCase):
    """Review blocker: --print-index-path must not fail before the index exists.

    CI calls it to decide whether to run --fetch --build; a cold cache must
    print the expected path and exit 0, never raise.
    """

    def test_cli_prints_expected_path_and_exits_zero_on_a_cold_cache(self):
        with tempfile.TemporaryDirectory() as name:
            root = Path(name)
            cache = root / "cache"
            expected = cache / "index-aaaaaaaa.bin"
            self.assertFalse(expected.exists())
            result = subprocess.run(
                [
                    sys.executable,
                    str(HYGIENE_DIR / "reference.py"),
                    "--print-index-path",
                    "--pin",
                    str(write_pin(root)),
                    "--cache",
                    str(cache),
                ],
                capture_output=True,
                text=True,
                timeout=120,
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(result.stdout.strip(), str(expected))

    def test_in_process_main_prints_the_path_and_returns_zero_on_a_cold_cache(self):
        with tempfile.TemporaryDirectory() as name:
            root = Path(name)
            sink = io.StringIO()
            with contextlib.redirect_stdout(sink):
                code = reference.main(
                    ["--print-index-path", "--pin", str(write_pin(root)), "--cache", str(root / "cache")]
                )
            self.assertEqual(code, 0)
            self.assertEqual(sink.getvalue().strip(), str(root / "cache" / "index-aaaaaaaa.bin"))

    def test_oracle_actions_still_refuse_a_missing_index(self):
        """The caller checks existence in CI; the oracle steps still validate."""
        with tempfile.TemporaryDirectory() as name:
            root = Path(name)
            sink = io.StringIO()
            with contextlib.redirect_stdout(sink), contextlib.redirect_stderr(io.StringIO()):
                code = reference.main(
                    ["--false-positives", "--pin", str(write_pin(root)), "--cache", str(root / "cache")]
                )
            self.assertEqual(code, 2)


class SelectionFillsRequestedCount(unittest.TestCase):
    """Review blocker: selection retries past short candidates until N is reached."""

    def test_retry_fills_the_requested_count_deterministically(self):
        with tempfile.TemporaryDirectory() as name:
            corpus = Path(name) / "corpus"
            (corpus / "python").mkdir(parents=True)
            (corpus / "javascript").mkdir()
            (corpus / "json").mkdir()
            for number in range(12):
                body = "\n".join(f"value_{number}_{line} = {line} + 2" for line in range(150))
                (corpus / "python" / f"module_{number}.py").write_text(body, encoding="utf-8")
                (corpus / "python" / f"short_{number}.py").write_text("x = 1\n", encoding="utf-8")
                body = "\n".join(f"const grid_{number}_{line} = {line};" for line in range(150))
                (corpus / "javascript" / f"grid_{number}.js").write_text(body, encoding="utf-8")
                (corpus / "json" / f"doctype_{number}.json").write_text(
                    json.dumps({"name": f"Doctype{number}", "fields": [{"i": line} for line in range(40)]}),
                    encoding="utf-8",
                )
            seed = reference.seed_from_entries([fake_entry()])
            chosen = reference.select_reference_files(corpus, 10, seed)
            # 10 files = 5 python + 2 javascript + 3 json, every quota filled
            # even though the python bucket holds as many too-short candidates
            # as usable ones (a single capped pool sample could miss the quota).
            self.assertEqual(len(chosen), 10)
            counts: dict[str, int] = {}
            for _, _, bucket in chosen:
                counts[bucket] = counts.get(bucket, 0) + 1
            self.assertEqual(counts, {"python": 5, "javascript": 2, reference.JSON_BUCKET: 3})
            self.assertFalse(any(rel.startswith("short_") for _, rel, _ in chosen))
            again = reference.select_reference_files(corpus, 10, seed)
            self.assertEqual([rel for _, rel, _ in chosen], [rel for _, rel, _ in again])


if __name__ == "__main__":
    unittest.main()