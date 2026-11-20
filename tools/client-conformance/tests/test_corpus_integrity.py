"""Integrity of the client conformance corpus (WP-CON-03.T68).

The corpus in tools/client-conformance/corpus/ is the executable specification every client of the
sans-IO core must reproduce. run.py --check only checks its structure. This file also checks that
(1) the structural check itself works, (2) case names are unique across files, (3) every variant of
the core's public types is exercised by some case, (4) the table of known gaps cannot go stale,
(5) the Rust enum parser used for (3) is sound, (6) files declare their feature the way the README
says. Stdlib only; no cargo, no network.
"""

from __future__ import annotations

import importlib.util
import json
import re
import sys
import tempfile
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
TOOL_DIR = HERE.parent
ROOT = TOOL_DIR.parents[1]
CORPUS_DIR = TOOL_DIR / "corpus"
TYPES_RS = ROOT / "crates" / "framework" / "spot-client-core" / "src" / "types.rs"


def load_run():
    """Import run.py by path (it is a script, not a package)."""
    spec = importlib.util.spec_from_file_location("client_conformance_run", TOOL_DIR / "run.py")
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module


run = load_run()

# Delegating variants: a tuple variant that wraps another of these enums. It has no shape of its
# own on the wire (the inner enum's own tag and fields are flattened in), so a case exercises it
# only by exercising the inner enum; requiring a case "for" it would be meaningless. Each entry is
# checked against types.rs (the variant exists and wraps exactly that type).
DELEGATING = {
    ("Input", "Request"): "Request",  # {"type": "request", "op": ...}: the op is the Request tag
    ("Request", "Realtime"): "RealtimeMsg",  # {"op": "realtime", "kind": ...}: the kind is the RealtimeMsg tag
    ("Output", "Emit"): "Event",  # {"type": "emit", "event": ...}: the event is the Event tag
}

# Which corpus set each enum is measured against (see seen_in_corpus).
SET_OF = {
    "Input": "in.type",
    "Request": "in.op",
    "RealtimeMsg": "in.kind",
    "Output": "out.type",
    "Event": "out.event",
    "DiscardReason": "out.reason",
}

# Variants no corpus case exercises yet, as {(enum, snake_case_variant): "reason; where it is tracked"}.
# Empty today. A stale entry (the variant is covered, or no longer exists) fails the test, so the
# table can only shrink.
UNCOVERED: dict[tuple[str, str], str] = {}

# ---------------------------------------------------------------------------------------------
# The Rust enum parser (small, by hand: types.rs is plain data definitions).
# ---------------------------------------------------------------------------------------------

ENUM_START = re.compile(r"^\s*pub enum (\w+)\s*(?:<[^>]*>)?\s*\{")
VARIANT = re.compile(r"^([A-Z][A-Za-z0-9_]*)\s*(?:\(\s*([A-Za-z0-9_:]+)\s*\))?\s*(?:\{|,|$)")


def snake_case(name: str) -> str:
    """serde's rename_all = "snake_case": an underscore before every capital except the first."""
    return re.sub(r"(?<!^)([A-Z])", r"_\1", name).lower()


def code_part(line: str) -> str:
    """The line without a trailing // comment (a doc comment is all comment)."""
    idx = line.find("//")
    return line if idx < 0 else line[:idx]


def parse_enums(text: str) -> dict[str, list[tuple[str, str | None]]]:
    """Return {enum name: [(variant, tuple_type_or_None), ...]} for every `pub enum` in `text`."""
    enums: dict[str, list[tuple[str, str | None]]] = {}
    lines = text.splitlines()
    i = 0
    while i < len(lines):
        start = ENUM_START.match(lines[i])
        i += 1
        if not start:
            continue
        name = start.group(1)
        variants: list[tuple[str, str | None]] = []
        depth = 1
        while i < len(lines) and depth > 0:
            raw = lines[i]
            i += 1
            stripped = raw.strip()
            if not stripped or stripped.startswith("//") or stripped.startswith("#"):
                continue
            if depth == 1:
                found = VARIANT.match(stripped)
                if found:
                    variants.append((found.group(1), found.group(2)))
            code = code_part(stripped)
            depth += code.count("{") - code.count("}")
        enums[name] = variants
    return enums


# ---------------------------------------------------------------------------------------------
# The corpus.
# ---------------------------------------------------------------------------------------------


def corpus_files() -> list[Path]:
    return sorted(CORPUS_DIR.glob("*.json"))


def load(path: Path) -> dict:
    return json.loads(path.read_text(encoding="utf-8"))


def seen_in_corpus() -> dict[str, set[str]]:
    seen: dict[str, set[str]] = {key: set() for key in ("in.type", "in.op", "in.kind", "out.type", "out.event", "out.reason")}

    def output(out: dict) -> None:
        if not isinstance(out, dict):
            return
        seen["out.type"].add(str(out.get("type")))
        if out.get("type") == "emit":
            seen["out.event"].add(str(out.get("event")))
            if "reason" in out:
                seen["out.reason"].add(str(out["reason"]))

    for path in corpus_files():
        for case in load(path).get("cases", []):
            for out in (case.get("start") or {}).get("out", []):
                output(out)
            for step in case.get("steps", []):
                inp = step.get("in", {})
                seen["in.type"].add(str(inp.get("type")))
                if inp.get("type") == "request":
                    seen["in.op"].add(str(inp.get("op")))
                    if inp.get("op") == "realtime":
                        seen["in.kind"].add(str(inp.get("kind")))
                for out in step.get("out", []):
                    output(out)
    return seen


def variants_to_cover(enums: dict[str, list[tuple[str, str | None]]]) -> dict[str, set[str]]:
    """The non-delegating variants of each measured enum, in the wire's snake_case."""
    wanted: dict[str, set[str]] = {}
    for enum in SET_OF:
        wanted[enum] = {
            snake_case(variant)
            for variant, _wrapped in enums.get(enum, [])
            if (enum, variant) not in DELEGATING
        }
    return wanted


def uncovered_now(enums, seen) -> dict[str, list[str]]:
    wanted = variants_to_cover(enums)
    return {
        enum: sorted(names - seen[SET_OF[enum]])
        for enum, names in wanted.items()
        if names - seen[SET_OF[enum]]
    }


# ---------------------------------------------------------------------------------------------
# Tests
# ---------------------------------------------------------------------------------------------

GOOD_CASE = {"name": "c", "steps": [{"in": {"type": "tick", "now_ms": 1}, "out": []}]}


class CheckCorpusFixtures(unittest.TestCase):
    """check_corpus() on fixture corpora in a temporary directory."""

    def setUp(self) -> None:
        self._tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self._tmp.cleanup)
        self.dir = Path(self._tmp.name)
        self._saved = run.CORPUS
        run.CORPUS = self.dir
        self.addCleanup(setattr, run, "CORPUS", self._saved)

    def write(self, name: str, doc) -> None:
        text = doc if isinstance(doc, str) else json.dumps(doc)
        (self.dir / name).write_text(text, encoding="utf-8")

    def problems(self) -> list[str]:
        return run.check_corpus()

    def assertProblem(self, file_name: str, needle: str | None = None) -> None:
        found = self.problems()
        self.assertTrue(found, "expected a problem, got none")
        joined = "\n".join(found)
        self.assertTrue(any(line.startswith(file_name) for line in found), joined)
        if needle is not None:
            self.assertIn(needle, joined)

    def test_well_formed_file_passes(self) -> None:
        self.write("a.json", {"topic": "t", "cases": [GOOD_CASE]})
        self.assertEqual(self.problems(), [])

    def test_well_formed_storage_file_with_start_passes(self) -> None:
        case = dict(GOOD_CASE, start={"site": "s1", "out": []})
        self.write("storage-a.json", {"topic": "t", "requires": ["storage"], "cases": [case]})
        self.assertEqual(self.problems(), [])

    def test_invalid_json(self) -> None:
        self.write("bad.json", "{not json")
        self.assertProblem("bad.json")

    def test_no_cases(self) -> None:
        self.write("a.json", {"topic": "t"})
        self.assertProblem("a.json", "non-empty 'cases'")

    def test_empty_cases(self) -> None:
        self.write("a.json", {"topic": "t", "cases": []})
        self.assertProblem("a.json", "non-empty 'cases'")

    def test_topic_not_a_string(self) -> None:
        self.write("a.json", {"topic": 7, "cases": [GOOD_CASE]})
        self.assertProblem("a.json", "string 'topic'")

    def test_unknown_required_feature(self) -> None:
        self.write("a.json", {"topic": "t", "requires": ["warp"], "cases": [GOOD_CASE]})
        self.assertProblem("a.json", "'requires'")

    def test_requires_not_a_list(self) -> None:
        self.write("a.json", {"topic": "t", "requires": "storage", "cases": [GOOD_CASE]})
        self.assertProblem("a.json", "'requires'")

    def test_case_without_name(self) -> None:
        self.write("a.json", {"topic": "t", "cases": [{"steps": GOOD_CASE["steps"]}]})
        self.assertProblem("a.json", "'name'")

    def test_case_with_empty_steps(self) -> None:
        self.write("a.json", {"topic": "t", "cases": [{"name": "c", "steps": []}]})
        self.assertProblem("a.json", "non-empty 'steps'")

    def test_step_without_in(self) -> None:
        self.write("a.json", {"topic": "t", "cases": [{"name": "c", "steps": [{"out": []}]}]})
        self.assertProblem("a.json", "step 0")

    def test_step_out_not_a_list(self) -> None:
        self.write("a.json", {"topic": "t", "cases": [{"name": "c", "steps": [{"in": {}, "out": {}}]}]})
        self.assertProblem("a.json", "list 'out'")

    def test_start_without_site(self) -> None:
        case = dict(GOOD_CASE, start={"out": []})
        self.write("a.json", {"topic": "t", "cases": [case]})
        self.assertProblem("a.json", "'start'")

    def test_start_with_non_list_out(self) -> None:
        case = dict(GOOD_CASE, start={"site": "s", "out": "x"})
        self.write("a.json", {"topic": "t", "cases": [case]})
        self.assertProblem("a.json", "'start'")

    def test_empty_directory(self) -> None:
        found = self.problems()
        self.assertEqual(len(found), 1)
        self.assertIn("no corpus files", found[0])

    def test_problem_in_one_file_does_not_hide_another(self) -> None:
        self.write("a.json", {"topic": "t", "cases": [GOOD_CASE]})
        self.write("b.json", {"topic": "t"})
        found = self.problems()
        self.assertEqual(len(found), 1)
        self.assertTrue(found[0].startswith("b.json"))


class RealCorpus(unittest.TestCase):
    def test_real_corpus_passes_the_structural_check(self) -> None:
        self.assertEqual(run.check_corpus(), [])

    def test_case_names_are_unique_across_the_corpus(self) -> None:
        where: dict[str, list[str]] = {}
        for path in corpus_files():
            for case in load(path)["cases"]:
                where.setdefault(case["name"], []).append(path.name)
        duplicates = {name: files for name, files in where.items() if len(files) > 1}
        self.assertEqual(
            duplicates,
            {},
            "case names must be unique across files (a runner that keys on the name would let a later "
            "case shadow an earlier one):\n"
            + "\n".join(f"  {name!r} in {', '.join(files)}" for name, files in sorted(duplicates.items())),
        )

    def test_every_variant_of_the_core_is_exercised(self) -> None:
        enums = parse_enums(TYPES_RS.read_text(encoding="utf-8"))
        missing = uncovered_now(enums, seen_in_corpus())
        for enum, names in list(missing.items()):
            known = {name for (e, name) in UNCOVERED if e == enum}
            names = [n for n in names if n not in known]
            if names:
                missing[enum] = names
            else:
                del missing[enum]
        message = "\n".join(
            f"    ({enum!r}, {name!r}): \"<reason; where it is tracked>\","
            for enum, names in sorted(missing.items())
            for name in names
        )
        self.assertEqual(
            missing,
            {},
            "no corpus case exercises these variants of spot-client-core's types; add a case, or paste "
            "into UNCOVERED in this file with a reason:\n" + message,
        )

    def test_the_uncovered_table_cannot_go_stale(self) -> None:
        enums = parse_enums(TYPES_RS.read_text(encoding="utf-8"))
        seen = seen_in_corpus()
        wanted = variants_to_cover(enums)
        for (enum, name), reason in sorted(UNCOVERED.items()):
            self.assertTrue(reason.strip(), f"UNCOVERED[{enum}, {name}] needs a reason")
            self.assertIn(enum, SET_OF, f"UNCOVERED names an enum this test does not measure: {enum}")
            self.assertIn(name, wanted[enum], f"UNCOVERED[{enum}, {name}] is not a variant in types.rs (or is delegating): remove it")
            self.assertNotIn(
                name, seen[SET_OF[enum]], f"UNCOVERED[{enum}, {name}] is now covered by the corpus: remove it"
            )

    def test_the_delegating_table_matches_the_code(self) -> None:
        enums = parse_enums(TYPES_RS.read_text(encoding="utf-8"))
        for (enum, variant), wrapped in DELEGATING.items():
            self.assertIn((variant, wrapped), enums.get(enum, []), f"{enum}::{variant}({wrapped}) not found in types.rs")
        # and no other tuple variant wraps one of the measured enums without being listed
        for enum, variants in enums.items():
            for variant, wrapped in variants:
                if wrapped in SET_OF and enum in SET_OF:
                    self.assertIn((enum, variant), DELEGATING, f"{enum}::{variant}({wrapped}) delegates; add it to DELEGATING")

    def test_files_declare_what_they_run(self) -> None:
        for path in corpus_files():
            doc = load(path)
            has_storage = "storage" in doc.get("requires", [])
            self.assertEqual(
                path.name.startswith("storage-"),
                has_storage,
                f"{path.name}: a storage-* file must have \"requires\": [\"storage\"], and only those files may",
            )

    def test_topics_are_unique_across_the_corpus(self) -> None:
        topics = {path.name: load(path)["topic"] for path in corpus_files()}
        self.assertEqual(
            duplicate_topics(topics),
            {},
            "every corpus file needs its own topic string; give the later file its own topic",
        )


def duplicate_topics(topics: dict[str, str]) -> dict[str, list[str]]:
    """{topic: [file names]} for every topic used by more than one file ({file name: topic} in)."""
    where: dict[str, list[str]] = {}
    for name, topic in sorted(topics.items()):
        where.setdefault(topic, []).append(name)
    return {topic: names for topic, names in where.items() if len(names) > 1}


class TopicUniqueness(unittest.TestCase):
    def test_a_planted_duplicate_is_reported(self) -> None:
        planted = {"a.json": "same", "b.json": "same", "c.json": "other"}
        self.assertEqual(duplicate_topics(planted), {"same": ["a.json", "b.json"]})

    def test_distinct_topics_pass(self) -> None:
        self.assertEqual(duplicate_topics({"a.json": "x", "b.json": "y"}), {})


class TypeParser(unittest.TestCase):
    SAMPLE = '''
use serde::Deserialize;

/// Docs with { braces } and a "pub enum Fake {" inside.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Sample {
    /// A struct variant.
    Alpha {
        /// A field.
        id: u64,
        /// A map: Vec<(String, String)>.
        headers: Vec<(String, String)>,
        value: Option<std::collections::HashMap<String, u8>>,
    },
    /// A tuple variant.
    Beta(Inner),
    /// A unit variant.
    Gamma,
    #[cfg(feature = "storage")]
    StorageThing {
        key: String, // trailing { comment
    },
    OneLiner { x: u8 },
    LongerName,
}

pub struct NotAnEnum {
    pub x: u8,
}

pub enum Second {
    Only,
}
'''

    def test_parses_every_shape(self) -> None:
        enums = parse_enums(self.SAMPLE)
        self.assertEqual(sorted(enums), ["Sample", "Second"])
        self.assertEqual(
            enums["Sample"],
            [
                ("Alpha", None),
                ("Beta", "Inner"),
                ("Gamma", None),
                ("StorageThing", None),
                ("OneLiner", None),
                ("LongerName", None),
            ],
        )
        self.assertEqual(enums["Second"], [("Only", None)])

    def test_snake_case_matches_serde(self) -> None:
        self.assertEqual(snake_case("SendHttp"), "send_http")
        self.assertEqual(snake_case("Tick"), "tick")
        self.assertEqual(snake_case("QueueDiscarded"), "queue_discarded")
        self.assertEqual(snake_case("StorageReply"), "storage_reply")
        self.assertEqual(snake_case("OwnerChanged"), "owner_changed")

    def test_real_file_yields_what_it_should(self) -> None:
        enums = parse_enums(TYPES_RS.read_text(encoding="utf-8"))
        for name in SET_OF:
            self.assertIn(name, enums, f"{name} not found in types.rs: did the file move or the parser break?")
        self.assertGreaterEqual(len(enums["Input"]), 5)
        self.assertGreaterEqual(len(enums["Request"]), 7)
        self.assertGreaterEqual(len(enums["RealtimeMsg"]), 3)
        self.assertGreaterEqual(len(enums["Output"]), 5)
        self.assertGreaterEqual(len(enums["Event"]), 12)
        self.assertGreaterEqual(len(enums["DiscardReason"]), 3)
        self.assertIn(("Request", "Request"), [("Request", v) for v, _ in enums["Input"]])

    def test_uncovered_detection_reports_a_gap(self) -> None:
        enums = {"Input": [("Tick", None), ("Brand", None), ("Request", "Request")]}
        seen = {key: set() for key in ("in.type", "in.op", "in.kind", "out.type", "out.event", "out.reason")}
        seen["in.type"] = {"tick", "request"}
        self.assertEqual(uncovered_now(enums, seen), {"Input": ["brand"]})


if __name__ == "__main__":
    unittest.main()
