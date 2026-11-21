"""Tests for tools/docsite/build.py (WP-DX-04.T1).

The suite builds the whole site from a tiny fixture contract tree under
``fixtures/mini`` (same layout as the repository, almost no content) and
asserts the things the contract cares about: every expected page exists,
every documented operation/schema/message/error code has its anchor, the
links resolve, and a repeat build is byte-identical. The checker itself is
exercised against hand-built trees that intentionally break.
"""
from __future__ import annotations

import re
import shutil
import sys
import tempfile
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
DOCSITE = HERE.parent
sys.path.insert(0, str(DOCSITE))

import build  # noqa: E402  (runs the dep bootstrap, then imports yaml/jinja2)

FIXTURE = HERE / "fixtures" / "mini"
REPO_ROOT = DOCSITE.parent.parent  # the repository checkout (…/WP-DX-04.T1)
COMMIT = "abc1234"


def out_dir(name: str) -> Path:
    tmp = Path(tempfile.mkdtemp(prefix=f"docsite-{name}-"))
    tmp.rmdir()  # build_site creates it (and refuses a foreign non-empty dir)
    return tmp


def build_fixture(name: str) -> tuple[build.Site, Path]:
    out = out_dir(name)
    site, _ = build.build_site(root=FIXTURE, out=out, skip_rustdoc=True,
                               commit=COMMIT)
    return site, out


def page(out: Path, rel: str) -> str:
    return (out / rel).read_text(encoding="utf-8")


class Extractors(unittest.TestCase):
    def test_envelope_code_takes_the_top_error(self):
        self.assertEqual(build._envelope_code({"error": {"code": "a.b"}}), "a.b")
        self.assertIsNone(build._envelope_code({"error": {"code": 7}}))
        self.assertIsNone(build._envelope_code({"code": "a.b"}))
        self.assertIsNone(build._envelope_code("stale"))

    def test_used_codes_follows_the_contract_walk(self):
        media = {"schema": {"$ref": "#/components/schemas/Error"},
                 "examples": {"named": {"value": {"error": {"code": "mini.slow"}}},
                              "other": {"value": {"pong": True}}}}
        spec = {
            "components": {"schemas": {"Envelope": {
                "examples": [{"error": {"code": "a.b"}},
                             {"error": {"code": "c.d"}}],
            }}},
            "paths": {"/x": {"post": {"responses": {"default": {
                "content": {"application/json": media}}}}}},
        }
        used = build.used_code_nodes(spec)
        self.assertEqual([code for _, code in used],
                         ["a.b", "c.d", "mini.slow"])  # walk order: schemas, paths

    def test_error_code_pattern_matches_the_contract(self):
        self.assertTrue(build.CODE_RE.fullmatch("document.stale_version"))
        self.assertTrue(build.CODE_RE.fullmatch("mini.bad_request"))
        self.assertFalse(build.CODE_RE.fullmatch("stale"))
        self.assertFalse(build.CODE_RE.fullmatch("UPPER.CASE"))

    def test_heading_slugs_are_deduped(self):
        text = build.add_heading_ids("<h2>Client messages</h2>"
                                     "<h2>Client messages</h2>")
        self.assertIn('id="client-messages"', text)
        self.assertIn('id="client-messages-2"', text)

class BuildFromFixture(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.site, cls.out = build_fixture("main")

    @classmethod
    def tearDownClass(cls):
        shutil.rmtree(cls.out, ignore_errors=True)

    def test_expected_pages_exist(self):
        expected = [
            "index.html",
            "native/index.html",
            "native/tag-health.html",
            "native/tag-resource.html",
            "native/schemas/index.html",
            "native/schemas/PingIn.html",
            "native/schemas/ErrorEnvelope.html",
            "realtime.html",
            "errors.html",
            "app-api.html",
            "events.html",
            "changelog/native-api.html",
            "changelog/app-api.html",
            "changelog/events.html",
            "adr/index.html",
            "adr/ADR-900-tiny-test-contract.html",
            "style.css",
            ".spot-docsite",
        ]
        for rel in expected:
            self.assertTrue((self.out / rel).exists(), rel)
        self.assertFalse((self.out / "storage.html").exists(),
                         "storage is absent from the fixture and gets no page")

    def test_registry_and_used_codes_get_anchors(self):
        rows = re.findall(r'id="(code-[^"]+)"', page(self.out, "errors.html"))
        self.assertEqual(set(rows),
                         {"code-mini.bad_request", "code-mini.retry"})

    def test_registered_op_and_schema_anchors_are_the_real_ones(self):
        self.assertEqual(sorted(self.site.required["op"]),
                         ["op-health_get", "op-ping_post"])
        self.assertIn("schema-PingIn", self.site.required["schema"])

    def test_realtime_messages_injected_under_their_sections(self):
        html = page(self.out, "realtime.html")
        self.assertIn('<h2 id="client-messages">', html)
        self.assertIn('<section class="msg" id="msg-ping">', html)
        self.assertIn('<section class="msg" id="msg-ready">', html)
        promised = {a.removeprefix("msg-") for a in self.site.required["msg"]}
        self.assertEqual(promised, {"ping", "pong", "ready", "joined"})

    def test_native_op_page_renders_params_body_and_errors(self):
        html = page(self.out, "native/tag-resource.html")
        self.assertIn("Ping the runtime", html)
        self.assertIn("name", html)
        # The shared error response is inlined on the contract overview.
        self.assertIn("#resp-Error", html)
        self.assertIn("mini.bad_request", page(self.out, "native/index.html"))

    def test_footer_shows_the_commit_and_credits_the_readme(self):
        html = page(self.out, "index.html")
        self.assertIn(COMMIT, html)
        self.assertIn("tools/docsite/README.md", html)

    def test_sidebar_marks_the_current_page(self):
        self.assertIn('aria-current="page"', page(self.out, "realtime.html"))

    def test_version_badges_come_from_version_files(self):
        self.assertIn("v0.2.0", page(self.out, "app-api.html"))
        self.assertIn("v0.1.0", page(self.out, "native/index.html"))

    def test_check_passes_on_the_whole_fixture_site(self):
        self.assertEqual(build.run_check(self.out, self.site), [])

    def test_build_pages_are_deterministic(self):
        other, other_out = build_fixture("repeat")
        self.assertEqual(self.site.pages, other.pages)
        shutil.rmtree(other_out, ignore_errors=True)

    def test_repeat_build_writes_identical_files(self):
        out2 = out_dir("disk")
        build.build_site(root=FIXTURE, out=out2, skip_rustdoc=True, commit=COMMIT)
        for rel in sorted(self.site.pages) + ["style.css", ".spot-docsite"]:
            self.assertEqual((self.out / rel).read_text(encoding="utf-8"),
                             (out2 / rel).read_text(encoding="utf-8"), rel)
        shutil.rmtree(out2, ignore_errors=True)


def tiny_out(name: str, pages: dict[str, str]) -> tuple[Path, build.Site]:
    """A hand-built tree for exercising run_check without a full build."""
    out = Path(tempfile.mkdtemp(prefix=f"docsite-{name}-"))
    for rel, text in pages.items():
        target = out / rel
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(text, encoding="utf-8")
    site = build.Site(FIXTURE, build.Environment(loader=build.FileSystemLoader(
        DOCSITE / "templates")))
    return out, site


class Checker(unittest.TestCase):
    def test_dead_internal_links_are_named(self):
        out, _ = tiny_out("deadlink", {
            "index.html": '<a href="nope.html">gone</a>',
            "sub/index.html": '<a href="../index.html#missing">frag</a>'})
        site = build.Site(FIXTURE, object())
        site.required["msg"].append("msg-ping")
        problems = build.run_check(out, site)
        joined = "\n".join(problems)
        self.assertIn("nope.html", joined)
        self.assertIn("#missing", joined)
        self.assertIn("msg-ping", joined)
        shutil.rmtree(out, ignore_errors=True)

    def test_clean_tree_passes(self):
        out, _ = tiny_out("clean", {"index.html": '<a href="sub/page.html#x">ok</a>',
                                    "sub/page.html": '<i id="x"></i><img src="../style.css">'})
        (out / "style.css").write_text("")
        self.assertEqual(build.run_check(out, build.Site(FIXTURE, None)), [])
        shutil.rmtree(out, ignore_errors=True)

    def test_rustdoc_targets_check_files_only(self):
        out, _ = tiny_out("rust", {"index.html":
                                   '<a href="rust/spot_app_api/index.html">rust</a>'})
        (out / "rust" / "spot_app_api").mkdir(parents=True)
        (out / "rust" / "spot_app_api" / "index.html").write_text("<html></html>")
        self.assertEqual(build.run_check(out, build.Site(FIXTURE, None)), [])
        shutil.rmtree(out, ignore_errors=True)


    def test_broken_ref_is_caught_and_named(self):
        """Rule from the brief: a fixture with a broken $ref fails --check."""
        broken = out_dir("broken-ref")
        shutil.copytree(FIXTURE, broken / "fixture")
        spec_path = (broken / "fixture" / "contracts" / "native-api"
                     / "openapi.yaml")
        spec_path.write_text(spec_path.read_text(encoding="utf-8").replace(
            '"#/components/schemas/PingIn"',
            '"#/components/schemas/Missing"'), encoding="utf-8")
        try:
            out = out_dir("broken")
            site, _ = build.build_site(root=broken / "fixture", out=out,
                                       skip_rustdoc=True)
            self.assertTrue(site.problems, "the broken ref must be recorded")
            problems = build.run_check(out, site)
            joined = "\n".join(site.problems + problems)
            self.assertIn("#/components/schemas/Missing", joined)
        finally:
            shutil.rmtree(out, ignore_errors=True)


class RealContract(unittest.TestCase):
    """The real contract tree builds and checks clean (fast path, no rustdoc)."""

    def test_real_contract_builds_and_checks(self):
        if not (REPO_ROOT / "contracts" / "native-api" / "openapi.yaml").exists():
            self.skipTest("real contract tree not present")
        out = out_dir("real")
        try:
            site, _ = build.build_site(root=REPO_ROOT, out=out,
                                       skip_rustdoc=True)
            self.assertEqual(build.run_check(out, site), [])
        finally:
            shutil.rmtree(out, ignore_errors=True)


class Guard(unittest.TestCase):
    def test_foreign_output_directory_is_refused(self):
        foreign = Path(tempfile.mkdtemp(prefix="docsite-foreign-"))
        (foreign / "important.txt").write_text("keep me")
        with self.assertRaises(SystemExit) as caught:
            build.build_site(root=FIXTURE, out=foreign, skip_rustdoc=True)
        self.assertIn(".spot-docsite", str(caught.exception))
        self.assertTrue((foreign / "important.txt").exists())
        shutil.rmtree(foreign, ignore_errors=True)

    def _run_main(self):  # helper, not a test
        out = out_dir("cli")
        rc = build.main(["--out", str(out), "--skip-rustdoc", "--check",
                         "--commit", COMMIT, "--root", str(FIXTURE)])
        return rc, out

    def test_main_builds_and_checks(self):
        rc, out = self._run_main()
        self.assertEqual(rc, 0)
        self.assertTrue((out / "index.html").exists())
        shutil.rmtree(out, ignore_errors=True)


if __name__ == "__main__":
    unittest.main()
# __MORE__