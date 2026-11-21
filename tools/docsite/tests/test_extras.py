"""Tests of the pages added in WP-DX-04.T2: the error catalogue, the UI extension contract, the
connector WIT, the formats, the guides, index.json and the freshness gate. Every case builds a
copy of the `mini` fixture with files added to it."""
from __future__ import annotations

import hashlib
import json
import posixpath
import shutil
import sys
import tempfile
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
DOCSITE = HERE.parent
sys.path.insert(0, str(DOCSITE))

import build  # noqa: E402
import extras  # noqa: E402
import wit  # noqa: E402

FIXTURE = HERE / "fixtures" / "mini"
GAPS = {"contract_codes_without_catalogue_entry": 2}

TWO_PREFIXES = '''
[[entry]]
code = "alpha.one"
class = "Validation"
summary = "The first thing."
explanation_key = "error.alpha.one"
params = ["field"]
remedy = "Fix it."
since = "0.1.0"

[[entry]]
code = "beta.two"
class = "none"
summary = "The second thing."
explanation_key = "error.beta.two"
params = []
remedy = "Retry."
since = "0.2.0"
'''

TINY_WIT = '''package acme:demo@0.1.0;

/// The demo world.
world demo {
    /// Colours.
    enum colour {
        red,
        green,
    }

    /// A pair.
    record pair {
        /// The left side.
        left: string,
        right: option<u32>,
    }

    /// Says hello.
    export hello: func(name: string) -> result<string, string>;
}
'''


def tree(extra: dict[str, str] | None = None) -> Path:
    root = Path(tempfile.mkdtemp(prefix="docsite-extras-")) / "repo"
    shutil.copytree(FIXTURE, root)
    for name, text in (extra or {}).items():
        path = root / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text, encoding="utf-8")
    return root


def build_tree(root: Path, gaps: dict[str, int] | None = None) -> tuple[build.Site, Path]:
    out = Path(tempfile.mkdtemp(prefix="docsite-out-"))
    out.rmdir()
    site, _ = build.build_site(root=root, out=out, skip_rustdoc=True, commit="abc1234",
                               known_gaps=gaps if gaps is not None else GAPS)
    return site, out


def page(out: Path, rel: str) -> str:
    return (out / rel).read_text(encoding="utf-8")


class ErrorCatalogue(unittest.TestCase):
    def test_pages_per_prefix_and_an_index(self):
        root = tree({"crates/framework/spot-errors/catalogue/ab.toml": TWO_PREFIXES})
        site, out = build_tree(root)
        self.assertEqual(site.problems, [])
        index = page(out, "errors/index.html")
        self.assertIn("alpha", index)
        self.assertIn("beta", index)
        alpha = page(out, "errors/alpha.html")
        self.assertIn('id="code-alpha.one"', alpha)
        self.assertIn("The first thing.", alpha)
        self.assertIn("ab.toml", alpha)
        self.assertIn("<code>field</code>", alpha)

    def test_a_code_defined_in_two_files_fails(self):
        root = tree({
            "crates/framework/spot-errors/catalogue/a.toml": TWO_PREFIXES,
            "crates/framework/spot-errors/catalogue/b.toml": TWO_PREFIXES.split("[[entry]]")[1].join(["[[entry]]", ""]),
        })
        site, _ = build_tree(root)
        self.assertTrue(any("alpha.one" in p and "both a.toml and b.toml" in p for p in site.problems),
                        site.problems)

    def test_contract_codes_without_an_entry_are_listed_and_counted(self):
        root = tree({"crates/framework/spot-errors/catalogue/ab.toml": TWO_PREFIXES})
        site, out = build_tree(root)
        index = page(out, "errors/index.html")
        # mini.bad_request and mini.retry are in the fixture's OpenAPI, not in the catalogue.
        self.assertIn("<code>mini.bad_request</code>", index)
        self.assertIn("(2; the known-gaps number is 2)", index)
        self.assertEqual(site.problems, [])

    def test_the_gap_number_may_not_grow(self):
        root = tree({"crates/framework/spot-errors/catalogue/ab.toml": TWO_PREFIXES})
        site, _ = build_tree(root, gaps={"contract_codes_without_catalogue_entry": 1})
        self.assertTrue(any("2 native API error codes have no catalogue entry" in p
                            for p in site.problems), site.problems)

    def test_a_missing_gap_number_is_a_problem(self):
        root = tree({"crates/framework/spot-errors/catalogue/ab.toml": TWO_PREFIXES})
        site, _ = build_tree(root, gaps={})
        self.assertTrue(any("known_gaps.toml has no" in p for p in site.problems))


class UiExtension(unittest.TestCase):
    SCHEMAS = {
        "contracts/ui-ext/VERSION": "0.1.0\n",
        "contracts/ui-ext/README.md": "# UI extension\n\nA readme.\n",
        "contracts/ui-ext/CHANGELOG.md": "# Changelog\n\n## 0.1.0\n\nFirst.\n",
        "contracts/ui-ext/errors.md": "# Errors\n\n| Code |\n| --- |\n| `uiext.schema` |\n",
        "contracts/ui-ext/schema/common.json": json.dumps({
            "title": "Common", "$defs": {"id": {"type": "string", "maxLength": 64}}}),
        "contracts/ui-ext/schema/action.json": json.dumps({
            "title": "Action", "description": "An action.",
            "required": ["id"], "additionalProperties": False,
            "properties": {"id": {"$ref": "common.json#/$defs/id"},
                           "kind": {"enum": ["call", "open"]},
                           "local": {"$ref": "#/$defs/target"}},
            "$defs": {"target": {"type": "object", "properties": {"x": {"type": "integer"}}}}}),
        "contracts/ui-ext/examples/valid/a.json": json.dumps({"$comment": "ok", "id": "x"}),
    }

    def test_overview_schema_tables_and_resolved_refs(self):
        site, out = build_tree(tree(self.SCHEMAS))
        self.assertEqual(site.problems, [])
        overview = page(out, "ui-ext.html")
        self.assertIn("v0.1.0", overview)
        action = page(out, "ui-ext/schema-action.html")
        self.assertIn("ui-ext/schema-common.html#def-id", action.replace("../", ""))
        self.assertIn("#def-target", action)
        self.assertIn("call", action)
        self.assertIn("Closed object", action)
        self.assertIn("uiext.schema", page(out, "ui-ext/errors.html"))
        self.assertIn("ok", page(out, "ui-ext/examples.html"))
        self.assertTrue((out / "changelog/ui-ext.html").exists())

    def test_a_broken_ref_fails(self):
        files = dict(self.SCHEMAS)
        files["contracts/ui-ext/schema/action.json"] = json.dumps({
            "title": "Action", "properties": {"id": {"$ref": "common.json#/$defs/nope"}}})
        site, _ = build_tree(tree(files))
        self.assertTrue(any("does not resolve" in p for p in site.problems), site.problems)
        files["contracts/ui-ext/schema/action.json"] = json.dumps({
            "title": "Action", "properties": {"id": {"$ref": "gone.json"}}})
        site, _ = build_tree(tree(files))
        self.assertTrue(any("schema file that does not exist" in p for p in site.problems))

    def test_an_example_that_is_not_json_fails(self):
        files = dict(self.SCHEMAS)
        files["contracts/ui-ext/examples/valid/bad.json"] = "{not json"
        site, _ = build_tree(tree(files))
        self.assertTrue(any("bad.json" in p and "not valid JSON" in p for p in site.problems))


class ConnectorWit(unittest.TestCase):
    PATH = "crates/framework/spot-connectors/wit/connector.wit"

    def test_the_outline_is_read_from_the_file(self):
        site, out = build_tree(tree({self.PATH: TINY_WIT}))
        self.assertEqual(site.problems, [])
        html = page(out, "wit/connector.html")
        self.assertIn("acme:demo@0.1.0", html)
        self.assertIn("The left side.", html)
        self.assertIn("Says hello.", html)
        self.assertIn("func(name: string) -&gt; result&lt;string, string&gt;", html)
        self.assertIn("package acme:demo@0.1.0;", html)  # the file, verbatim

    def test_an_unreadable_construct_fails_the_build(self):
        broken = TINY_WIT.replace("export hello", "resource handle {\n    }\n    export hello")
        site, _ = build_tree(tree({self.PATH: broken}))
        self.assertTrue(any("cannot read" in p and "connector.wit" in p for p in site.problems),
                        site.problems)

    def test_the_reader_names_the_line(self):
        with self.assertRaises(wit.WitError) as caught:
            wit.parse("package a:b@0.1.0;\nworld w {\n  nonsense here;\n}\n")
        self.assertIn("line 3", str(caught.exception))


class GuidesAndFormats(unittest.TestCase):
    def test_guides_are_rendered_and_links_between_them_work(self):
        root = tree({
            "docs/running-spot.md": "# Running\n\nSee [files](running-spot/files.md#top).\n",
            "docs/running-spot/files.md": "# Files\n\nBack to [the guide](../running-spot.md).\n",
        })
        site, out = build_tree(root)
        self.assertEqual(site.problems, [])
        self.assertIn('href="../guides/files.html#top"', page(out, "guides/running-spot.html"))
        self.assertIn('href="../guides/running-spot.html"', page(out, "guides/files.html"))
        self.assertIn("Files", page(out, "guides/index.html"))

    def test_a_link_to_a_file_the_site_does_not_render_fails(self):
        # The guide sits one level deeper in the private repo than in the public
        # tree, so the link to the fixture ADR is computed from where the guide is.
        adr = posixpath.relpath("docs/adr/ADR-900-tiny-test-contract.md",
                                posixpath.dirname(extras.GUIDE_INDEX))
        root = tree({
            extras.GUIDE_INDEX: f"# Running\n\n[gone](running-spot/gone.md) and [other]({adr}).\n",
        })
        site, _ = build_tree(root)
        self.assertTrue(any("gone.md" in p and "does not exist" in p for p in site.problems))
        self.assertTrue(any("ADR-900" in p and "does not render" in p.replace("site does not render", "does not render")
                            for p in site.problems), site.problems)

    def test_manifest_and_field_kind_pages(self):
        root = tree({
            "crates/framework/spot-manifest/FORMAT.md": "# `spot-app.toml`\n\nOne file per app.\n",
            "crates/framework/spot-meta/src/definition.rs":
                "pub enum FieldType {\n    /// True or false.\n    #[serde(rename = \"bool\")]\n    Bool,\n"
                "    /// Short text.\n    #[serde(rename = \"text\")]\n    Text,\n}\n",
        })
        site, out = build_tree(root)
        self.assertEqual(site.problems, [])
        self.assertIn("One file per app.", page(out, "formats/manifest.html"))
        kinds = page(out, "formats/doctype.html")
        self.assertIn('id="kind-bool"', kinds)
        self.assertIn("Short text.", kinds)

    def test_a_field_kind_source_that_yields_nothing_fails(self):
        root = tree({"crates/framework/spot-meta/src/definition.rs": "pub enum FieldType {\n    Bool,\n}\n"})
        site, _ = build_tree(root)
        self.assertTrue(any("no field kind with a serde name" in p for p in site.problems))


class GenericContractsAndFreshness(unittest.TestCase):
    OPENAPI = "openapi: 3.1.0\ninfo:\n  title: t\n  version: 0.2.0\n  description: The agent channel.\n" \
              "paths:\n  /ping:\n    get:\n      operationId: ping\n      summary: Ping.\n      responses: {}\n"

    def test_a_contract_with_a_version_file_gets_a_page_with_its_version(self):
        root = tree({"contracts/agent-channel/VERSION": "0.2.0\n",
                     "contracts/agent-channel/openapi.yaml": self.OPENAPI,
                     "contracts/agent-channel/CHANGELOG.md": "# Changes\n"})
        site, out = build_tree(root)
        self.assertEqual(site.problems, [])
        html = page(out, "agent-channel.html")
        self.assertIn("v0.2.0", html)
        self.assertIn("<code>ping</code>", html)
        self.assertTrue((out / "changelog/agent-channel.html").exists())

    def test_a_contract_without_a_page_fails_naming_the_directory(self):
        root = tree()
        site, _ = build_tree(root)
        extras = site.gen_extras  # the Extras of that build
        del site.pages["app-api.html"]
        extras._freshness()
        self.assertTrue(any("'app-api'" in p and "no page" in p for p in site.problems), site.problems)

    def test_a_wrong_version_badge_fails(self):
        site, _ = build_tree(tree())
        version = (FIXTURE / "contracts" / "app-api" / "VERSION").read_text().strip()
        site.pages["app-api.html"] = site.pages["app-api.html"].replace(f"v{version}", "v0.0.1")
        site.gen_extras._freshness()
        self.assertTrue(any("does not show version" in p for p in site.problems))


class IndexAndDeterminism(unittest.TestCase):
    EXTRA = {
        "crates/framework/spot-errors/catalogue/ab.toml": TWO_PREFIXES,
        "crates/framework/spot-connectors/wit/connector.wit": TINY_WIT,
        "docs/running-spot.md": "# Running\n",
    }

    def test_index_json_is_sorted_and_names_written_pages(self):
        _site, out = build_tree(tree(self.EXTRA))
        entries = json.loads(page(out, "index.json"))
        self.assertEqual(entries, sorted(entries, key=lambda e: (e["section"], e["path"])))
        paths = {e["path"] for e in entries}
        self.assertIn("errors/alpha.html", paths)
        self.assertIn("wit/connector.html", paths)
        self.assertTrue(all((out / p).exists() for p in paths))
        self.assertTrue(all(set(e) == {"title", "path", "section"} for e in entries))

    def test_two_builds_are_byte_identical(self):
        root = tree(self.EXTRA)
        _a, out_a = build_tree(root)
        _b, out_b = build_tree(root)

        def digest(out: Path) -> dict[str, str]:
            return {p.relative_to(out).as_posix(): hashlib.sha256(p.read_bytes()).hexdigest()
                    for p in sorted(out.rglob("*")) if p.is_file()}

        self.assertEqual(digest(out_a), digest(out_b))

    def test_the_real_repository_builds_and_checks_with_the_known_gaps_as_they_are(self):
        out = Path(tempfile.mkdtemp(prefix="docsite-real-"))
        out.rmdir()
        site, _ = build.build_site(root=build.REPO_ROOT, out=out, skip_rustdoc=True, commit="abc1234")
        self.assertEqual(site.problems + build.run_check(out, site), [])


if __name__ == "__main__":
    unittest.main()
