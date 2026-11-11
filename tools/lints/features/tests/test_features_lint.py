from __future__ import annotations

import contextlib
import io
import shutil
import sys
import tempfile
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE.parent))
import features_lint  # noqa: E402

RULES = {
    "feature": {"name": "testing"},
    "framework": {"prefix": "crates/framework/"},
    "non_product": {"packages": {"fake-server": "a stand-in server for tests"}},
    "allow_edge": [],
}


def package(root: Path, folder: str, name: str, features=None, deps=None, bins=()):
    """A `cargo metadata --no-deps` package entry."""
    targets = [{"kind": ["lib"], "name": name}] + [{"kind": ["bin"], "name": b} for b in bins]
    return {
        "name": name,
        "id": f"{name} 0.1.0 (path+file://{root}/{folder}/{name})",
        "manifest_path": str(root / folder / name / "Cargo.toml"),
        "features": features or {},
        "dependencies": deps or [],
        "targets": targets,
    }


def dep(name, kind=None, features=(), default=True):
    return {"name": name, "kind": kind, "features": list(features), "uses_default_features": default}


def baseline_of(rules: dict) -> dict:
    """The baseline that matches `rules` exactly."""
    names, edges = features_lint.current_lists(rules)
    return {
        "non_product": {"count": len(names), "packages": names},
        "allow_edge": {"count": len(edges), "edges": edges},
    }


def metadata(root: Path, packages: list[dict]) -> dict:
    return {"packages": packages, "workspace_members": [p["id"] for p in packages]}


class EdgeTests(unittest.TestCase):
    root = Path("/repo")

    def edges(self, packages, rules=RULES):
        return features_lint.edge_findings(metadata(self.root, packages), self.root, rules)

    def framework(self, name="fw", features=None):
        return package(self.root, "crates/framework", name, features or {"testing": []})

    def test_a_normal_dependency_enabling_the_feature_is_a_finding(self):
        app = package(self.root, "crates/apps", "app", deps=[dep("fw", features=["testing"])])
        self.assertEqual(self.edges([self.framework(), app]), [("app", "fw", "normal")])

    def test_a_build_dependency_is_a_finding_too(self):
        app = package(self.root, "crates/apps", "app", deps=[dep("fw", kind="build", features=["testing"])])
        self.assertEqual(self.edges([self.framework(), app]), [("app", "fw", "build")])

    def test_a_dev_dependency_is_fine_and_so_is_a_self_dev_dependency(self):
        fw = self.framework()
        fw["dependencies"] = [dep("fw", kind="dev", features=["testing"])]
        app = package(self.root, "crates/apps", "app", deps=[dep("fw", kind="dev", features=["testing"])])
        self.assertEqual(self.edges([fw, app]), [])

    def test_a_dependency_without_the_feature_is_fine(self):
        app = package(self.root, "crates/apps", "app", deps=[dep("fw")])
        self.assertEqual(self.edges([self.framework(), app]), [])

    def test_a_feature_that_turns_it_on_counts(self):
        fw = self.framework(features={"testing": [], "fakes": ["testing"]})
        app = package(self.root, "crates/apps", "app", deps=[dep("fw", features=["fakes"])])
        self.assertEqual(self.edges([fw, app]), [("app", "fw", "normal")])

    def test_a_default_feature_that_turns_it_on_counts(self):
        fw = self.framework(features={"testing": [], "default": ["testing"]})
        app = package(self.root, "crates/apps", "app", deps=[dep("fw")])
        self.assertEqual(self.edges([fw, app]), [("app", "fw", "normal")])
        off = package(self.root, "crates/apps", "off", deps=[dep("fw", default=False)])
        self.assertEqual(self.edges([fw, off]), [])

    def test_a_dependency_on_a_crate_outside_the_framework_is_not_checked(self):
        backend = package(self.root, "crates/backends", "be", features={"testing": []})
        app = package(self.root, "crates/apps", "app", deps=[dep("be", features=["testing"])])
        self.assertEqual(self.edges([backend, app]), [])

    def test_another_feature_name_is_fine(self):
        fw = self.framework(features={"extra": []})
        app = package(self.root, "crates/apps", "app", deps=[dep("fw", features=["extra"])])
        self.assertEqual(self.edges([fw, app]), [])


class BinaryTests(unittest.TestCase):
    root = Path("/repo")

    def test_a_package_with_a_binary_is_a_product_unless_listed(self):
        packages = [
            package(self.root, "crates/apps", "app", bins=["app"]),
            package(self.root, "crates/apps", "fake-server", bins=["fake"]),
            package(self.root, "crates/framework", "lib"),
        ]
        self.assertEqual(
            features_lint.product_binaries(metadata(self.root, packages), RULES), ["app"]
        )

    def test_the_tree_listing_is_read_by_package_and_feature(self):
        listing = "\n".join(
            [
                "app v0.1.0 (/repo/crates/apps/app)|",
                "fw v0.1.0 (/repo/crates/framework/fw)|testing,extra",
                "other v0.1.0 (/repo/crates/framework/other)|extra (*)",
                "serde v1.0.0|testing",
            ]
        )
        self.assertEqual(
            features_lint.tree_findings(listing, {"fw", "other"}, "testing"), ["fw"]
        )

    def test_a_proc_macro_line_is_read(self):
        listing = "fw-macros v0.1.0 (proc-macro) (/repo/crates/framework/fw-macros)|testing"
        self.assertEqual(
            features_lint.tree_findings(listing, {"fw-macros"}, "testing"), ["fw-macros"]
        )


class RunBase:
    """`run` against synthetic metadata and tree listings."""

    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.root = Path(self.temporary.name)
        (self.root / "Cargo.toml").write_text("[workspace]\n", encoding="utf-8")

    def tearDown(self):
        self.temporary.cleanup()

    def go(self, packages, rules, listing="", unified=None, baseline="matching", trees=None):
        """`trees` maps a binary to its own listing; `unified` is the listing of the whole build."""
        if baseline == "matching":
            baseline = baseline_of(rules)
        output = io.StringIO()
        with contextlib.redirect_stdout(output), contextlib.redirect_stderr(output):
            code = features_lint.run(
                self.root,
                rules,
                metadata=lambda _root: metadata(self.root, packages),
                tree=lambda _root, name: (trees or {}).get(name, listing),
                unified=lambda _root, _names: listing if unified is None else unified,
                baseline=baseline,
            )
        return code, output.getvalue()

    def packages(self):
        fw = package(self.root, "crates/framework", "fw", {"testing": []})
        app = package(self.root, "crates/apps", "app", deps=[dep("fw", features=["testing"])], bins=["app"])
        return fw, app


class RunTests(RunBase, unittest.TestCase):
    def test_a_clean_tree_passes(self):
        fw, _app = self.packages()
        app = package(self.root, "crates/apps", "app", deps=[dep("fw")], bins=["app"])
        code, output = self.go([fw, app], RULES, "app v0.1.0 (x)|\nfw v0.1.0 (y)|")
        self.assertEqual(code, 0, output)

    def test_an_edge_finding_fails_and_names_both_crates(self):
        code, output = self.go(list(self.packages()), RULES)
        self.assertEqual(code, 1, output)
        self.assertIn("app: a normal dependency on fw", output)

    def test_a_listed_edge_passes_and_a_stale_entry_fails(self):
        rules = dict(RULES, allow_edge=[{"package": "app", "dependency": "fw", "reason": "debt"}])
        code, output = self.go(list(self.packages()), rules)
        self.assertEqual(code, 0, output)
        fw, _app = self.packages()
        clean = package(self.root, "crates/apps", "app", deps=[dep("fw")], bins=["app"])
        code, output = self.go([fw, clean], rules)
        self.assertEqual(code, 1, output)
        self.assertIn("no longer exists", output)

    def test_a_product_binary_that_resolves_the_feature_fails(self):
        fw, _app = self.packages()
        app = package(self.root, "crates/apps", "app", deps=[dep("fw")], bins=["app"])
        code, output = self.go([fw, app], RULES, "app v0.1.0 (x)|\nfw v0.1.0 (y)|testing")
        self.assertEqual(code, 1, output)
        self.assertIn("a product binary build enables `testing` on the framework crate fw", output)

    def test_no_workspace_is_not_a_failure(self):
        (self.root / "Cargo.toml").unlink()
        code, output = self.go([], RULES)
        self.assertEqual(code, 0, output)


class UnifiedTests(RunBase, unittest.TestCase):
    """The unified product build, the scope of the check and the fail-closed paths (synthetic)."""

    def two_apps(self, folder_b="crates/apps"):
        fw = package(self.root, "crates/framework", "fw", {"testing": []})
        a = package(self.root, "crates/apps", "a", deps=[dep("fw")], bins=["a"])
        b = package(self.root, folder_b, "b", deps=[dep("fw")], bins=["b"])
        return [fw, a, b]

    def test_a_neighbour_binary_that_unifies_the_feature_is_reported_for_the_other_one(self):
        # On their own both resolve fw without `testing`; built together the feature is on.
        alone = "a v0.1.0 (x)|\nfw v0.1.0 (y)|"
        trees = {"a": alone, "b": "b v0.1.0 (x)|\nfw v0.1.0 (y)|testing"}
        code, output = self.go(self.two_apps(), RULES, unified="a v0.1.0 (x)|\nb v0.1.0 (x)|\nfw v0.1.0 (y)|testing", trees=trees)
        self.assertEqual(code, 1, output)
        self.assertIn("a: in the unified product build", output)
        self.assertIn("by b", output)
        self.assertIn("b: a product binary build enables `testing`", output)

    def test_the_unified_build_without_the_feature_is_clean(self):
        code, output = self.go(self.two_apps(), RULES, listing="fw v0.1.0 (y)|")
        self.assertEqual(code, 0, output)

    def test_a_binary_under_apps_that_enables_the_feature_is_reported(self):
        packages = self.two_apps("apps")
        code, output = self.go(packages, RULES, listing="fw v0.1.0 (y)|testing")
        self.assertEqual(code, 1, output)
        self.assertIn("b: a product binary build enables `testing`", output)

    def test_a_binary_under_tools_with_a_normal_edge_is_reported(self):
        fw = package(self.root, "crates/framework", "fw", {"testing": []})
        tool = package(self.root, "tools", "t", deps=[dep("fw", features=["testing"])], bins=["t"])
        code, output = self.go([fw, tool], RULES)
        self.assertEqual(code, 1, output)
        self.assertIn("t: a normal dependency on fw", output)

    def test_an_unreadable_tree_line_fails_closed(self):
        code, output = self.go(self.two_apps(), RULES, listing="this is not a tree line")
        self.assertEqual(code, 2, output)
        self.assertIn("cannot read this line", output)

    def test_an_unreadable_unified_listing_fails_closed(self):
        code, output = self.go(self.two_apps(), RULES, unified="garbage")
        self.assertEqual(code, 2, output)

    def test_a_missing_baseline_fails(self):
        code, output = self.go(self.two_apps(), RULES, listing="fw v0.1.0 (y)|", baseline=None)
        self.assertEqual(code, 1, output)
        self.assertIn("baseline.toml: missing", output)


class BaselineTests(RunBase, unittest.TestCase):
    """The `non_product` and `[[allow_edge]]` lists may not grow without a baseline change."""

    def clean(self):
        fw = package(self.root, "crates/framework", "fw", {"testing": []})
        app = package(self.root, "crates/apps", "app", deps=[dep("fw")], bins=["app"])
        return [fw, app]

    def with_non_product(self, *names):
        return dict(RULES, non_product={"packages": {n: "a reason" for n in names}})

    def test_a_new_non_product_entry_without_a_baseline_change_fails(self):
        rules = self.with_non_product("fake-server", "sneaky")
        baseline = baseline_of(RULES)  # still only fake-server
        code, output = self.go(self.clean(), rules, "fw v0.1.0 (y)|", baseline=baseline)
        self.assertEqual(code, 1, output)
        self.assertIn("non_product entry `sneaky` is not in baseline.toml", output)
        self.assertIn("non_product grew", output)

    def test_the_same_entry_with_a_baseline_change_passes(self):
        rules = self.with_non_product("fake-server", "sneaky")
        code, output = self.go(self.clean(), rules, "fw v0.1.0 (y)|", baseline=baseline_of(rules))
        self.assertEqual(code, 0, output)

    def test_a_new_allow_edge_without_a_baseline_change_fails(self):
        fw = package(self.root, "crates/framework", "fw", {"testing": []})
        app = package(self.root, "crates/apps", "app", deps=[dep("fw", features=["testing"])], bins=["app"])
        rules = dict(RULES, allow_edge=[{"package": "app", "dependency": "fw", "reason": "debt"}])
        code, output = self.go([fw, app], rules, "fw v0.1.0 (y)|", baseline=baseline_of(RULES))
        self.assertEqual(code, 1, output)
        self.assertIn("allow_edge entry `app -> fw` is not in baseline.toml", output)

    def test_removing_an_entry_passes_and_asks_for_the_regenerate_command(self):
        rules = dict(RULES, non_product={"packages": {}})
        code, output = self.go(self.clean(), rules, "fw v0.1.0 (y)|", baseline=baseline_of(RULES))
        self.assertEqual(code, 0, output)
        self.assertIn("--regenerate-baseline", output)

    def test_a_non_product_entry_without_a_reason_fails(self):
        rules = dict(RULES, non_product={"packages": {"fake-server": " "}})
        code, output = self.go(self.clean(), rules, "fw v0.1.0 (y)|", baseline=baseline_of(rules))
        self.assertEqual(code, 1, output)
        self.assertIn("`fake-server` has no reason", output)

    def test_a_list_form_of_non_product_fails(self):
        rules = dict(RULES, non_product={"packages": ["fake-server"]})
        code, output = self.go(self.clean(), rules, "fw v0.1.0 (y)|", baseline=baseline_of(rules))
        self.assertEqual(code, 1, output)

    def test_a_baseline_with_a_wrong_count_or_a_missing_section_fails(self):
        baseline = baseline_of(RULES)
        baseline["non_product"]["count"] = 0
        code, output = self.go(self.clean(), RULES, "fw v0.1.0 (y)|", baseline=baseline)
        self.assertEqual(code, 1, output)
        self.assertIn("count 0 does not match", output)
        code, output = self.go(self.clean(), RULES, "fw v0.1.0 (y)|", baseline={})
        self.assertEqual(code, 1, output)
        self.assertIn("section is missing or malformed", output)

    def test_the_regenerated_baseline_matches_the_rules_and_shrinks_only_that_way(self):
        rules = self.with_non_product("b-tool", "a-tool")
        text = features_lint.baseline_text(rules)
        import tomllib

        parsed = tomllib.loads(text)
        self.assertEqual(parsed["non_product"], {"count": 2, "packages": ["a-tool", "b-tool"]})
        self.assertEqual(parsed["allow_edge"], {"count": 0, "edges": []})
        self.assertEqual(features_lint.baseline_problems(rules, parsed), [])

    def test_an_unreadable_baseline_file_is_an_error(self):
        bad = self.root / "baseline.toml"
        bad.write_text("not = [valid", encoding="utf-8")
        with self.assertRaises(features_lint.Unreadable):
            features_lint.load_baseline(bad)


class RepositoryTests(unittest.TestCase):
    """(e) The repository itself is clean: committed baseline matches rules.toml and has reasons."""

    def test_the_committed_baseline_covers_the_committed_rules(self):
        rules = features_lint.load_rules()
        baseline = features_lint.load_baseline()
        self.assertEqual(features_lint.baseline_problems(rules, baseline), [])
        self.assertEqual(features_lint.reason_problems(rules), [])
        self.assertEqual(baseline, __import__("tomllib").loads(features_lint.baseline_text(rules)))

    @unittest.skipUnless(shutil.which("cargo"), "cargo is not installed")
    def test_the_repository_passes_the_whole_lint(self):
        output = io.StringIO()
        with contextlib.redirect_stdout(output), contextlib.redirect_stderr(output):
            code = features_lint.run(HERE.parents[3])
        self.assertEqual(code, 0, output.getvalue())


@unittest.skipUnless(shutil.which("cargo"), "cargo is not installed")
class CargoTests(unittest.TestCase):
    """The same checks against a real two-crate workspace, resolved by cargo itself."""

    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.root = Path(self.temporary.name)
        self.write("Cargo.toml", '[workspace]\nmembers = ["crates/*/*"]\nresolver = "3"\n')
        self.write(
            "crates/framework/fw/Cargo.toml",
            '[package]\nname = "fw"\nversion = "0.1.0"\nedition = "2024"\n'
            '[features]\ntesting = []\n'
            '[dev-dependencies]\nfw = { path = ".", features = ["testing"] }\n',
        )
        self.write("crates/framework/fw/src/lib.rs", "")

    def tearDown(self):
        self.temporary.cleanup()

    def write(self, relative: str, text: str) -> None:
        path = self.root / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text, encoding="utf-8")

    def app(self, dependency: str, dev: str = "") -> None:
        self.write(
            "crates/apps/app/Cargo.toml",
            '[package]\nname = "app"\nversion = "0.1.0"\nedition = "2024"\n'
            f'[dependencies]\n{dependency}\n{dev}\n',
        )
        self.write("crates/apps/app/src/main.rs", "fn main() {}\n")

    def lint(self) -> tuple[int, str]:
        rules = dict(RULES, non_product={"packages": {}})
        output = io.StringIO()
        with contextlib.redirect_stdout(output), contextlib.redirect_stderr(output):
            code = features_lint.run(self.root, rules, baseline=baseline_of(rules))
        return code, output.getvalue()

    def test_the_self_dev_dependency_alone_is_clean(self):
        self.app('fw = { path = "../../framework/fw" }')
        code, output = self.lint()
        self.assertEqual(code, 0, output)

    def test_a_dev_dependency_of_a_product_is_clean(self):
        self.app(
            "",
            '[dev-dependencies]\nfw = { path = "../../framework/fw", features = ["testing"] }',
        )
        code, output = self.lint()
        self.assertEqual(code, 0, output)

    def test_a_normal_dependency_with_the_feature_fails_the_edge_and_the_binary(self):
        self.app('fw = { path = "../../framework/fw", features = ["testing"] }')
        code, output = self.lint()
        self.assertEqual(code, 1, output)
        self.assertIn("app: a normal dependency on fw", output)
        self.assertIn("a product binary build enables `testing` on the framework crate fw", output)

    def second_app(self, name: str, dependency: str, folder: str = "apps") -> None:
        self.write(
            f"crates/{folder}/{name}/Cargo.toml",
            f'[package]\nname = "{name}"\nversion = "0.1.0"\nedition = "2024"\n'
            f'[dependencies]\n{dependency}\n',
        )
        self.write(f"crates/{folder}/{name}/src/main.rs", "fn main() {}\n")

    def test_a_neighbour_binary_unifies_the_feature_onto_another_binary(self):
        # a never asks for `testing`; b does. Alone, a is clean; built together it is not.
        self.second_app("a", 'fw = { path = "../../framework/fw" }')
        self.second_app("b", 'fw = { path = "../../framework/fw", features = ["testing"] }')
        code, output = self.lint()
        self.assertEqual(code, 1, output)
        self.assertIn("a: in the unified product build", output)
        self.assertIn("by b", output)
        self.assertNotIn("a: a product binary build", output)

    def test_without_the_neighbour_the_first_binary_is_clean(self):
        self.second_app("a", 'fw = { path = "../../framework/fw" }')
        self.second_app("b", 'fw = { path = "../../framework/fw" }')
        code, output = self.lint()
        self.assertEqual(code, 0, output)

    def test_a_binary_under_apps_that_enables_the_feature_is_reported(self):
        self.write("Cargo.toml", '[workspace]\nmembers = ["crates/*/*", "apps/*"]\nresolver = "3"\n')
        self.write(
            "apps/shop/Cargo.toml",
            '[package]\nname = "shop"\nversion = "0.1.0"\nedition = "2024"\n'
            '[dependencies]\nfw = { path = "../../crates/framework/fw", features = ["testing"] }\n',
        )
        self.write("apps/shop/src/main.rs", "fn main() {}\n")
        code, output = self.lint()
        self.assertEqual(code, 1, output)
        self.assertIn("shop: a normal dependency on fw", output)

    def test_an_unparsable_manifest_fails_closed(self):
        self.write("crates/apps/broken/Cargo.toml", "[package\nname = ")
        code, output = self.lint()
        self.assertEqual(code, 2, output)

    def test_a_default_feature_chain_is_caught_in_the_binary_build(self):
        self.write(
            "crates/framework/fw/Cargo.toml",
            '[package]\nname = "fw"\nversion = "0.1.0"\nedition = "2024"\n'
            '[features]\ntesting = []\ndefault = ["testing"]\n',
        )
        self.app('fw = { path = "../../framework/fw" }')
        code, output = self.lint()
        self.assertEqual(code, 1, output)
        self.assertIn("app: a normal dependency on fw", output)


if __name__ == "__main__":
    unittest.main()
