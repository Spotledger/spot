"""Corpus tests for the effects lint: synthetic controllers, each with the verdict it must get."""

from __future__ import annotations

import sys
import tempfile
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE.parent))
import check  # noqa: E402

HEADER = """
use spot_app_api::{Controller, HookContext};
struct Ctl;
impl Controller for Ctl {
    fn doctype(&self) -> &str { "invoice" }
    fn steps(&self) -> Vec<StepDecl> { Vec::new() }
    fn run_step<'a>(&'a self, step: &'a StepName, ctx: &'a mut dyn HookContext) -> BoxFuture<'a, AppResult<()>> {
        match step.as_str() {
            "shop:notify_committed" => Box::pin(self.notify(ctx)),
            "shop:check" => Box::pin(async { Ok(()) }),
            _ => Box::pin(async { Ok(()) }),
        }
    }
}
"""


def codes(source: str) -> list[str]:
    return [v.code for v in check.scan_source("corpus.rs", source)]


def lines(source: str) -> list[int]:
    return [v.line for v in check.scan_source("corpus.rs", source)]


class DirectCalls(unittest.TestCase):
    def test_each_forbidden_api_is_flagged(self) -> None:
        for api in (
            "spot_egress::send(req)",
            "reqwest::Client::new()",
            "spot_mail::deliver(m)",
            "lettre::Message::builder()",
            "std::net::TcpStream::connect(a)",
            "tokio::net::TcpStream::connect(a)",
            "spot_webhooks::fire(h)",
        ):
            with self.subTest(api=api):
                src = HEADER + f"impl Ctl {{ async fn notify(&self, _c: &mut dyn HookContext) -> AppResult<()> {{ let _ = {api}; Ok(()) }} }}\n"
                self.assertEqual(codes(src), [check.CODE_DIRECT])

    def test_a_clean_controller_passes(self) -> None:
        src = HEADER + "impl Ctl { async fn notify(&self, c: &mut dyn HookContext) -> AppResult<()> { c.enqueue_job(job())?; Ok(()) } }\n"
        self.assertEqual(codes(src), [])

    def test_a_file_that_is_not_a_controller_is_not_scanned(self) -> None:
        src = "fn send() { let _ = reqwest::Client::new(); }\n"
        self.assertEqual(codes(src), [])

    def test_comments_and_strings_do_not_count(self) -> None:
        src = HEADER + '''
// reqwest::get is forbidden here
/* spot_mail::send in a block comment */
impl Ctl {
    async fn notify(&self, _c: &mut dyn HookContext) -> AppResult<()> {
        let _doc = "calls lettre and std::net in prose";
        let _raw = r#"tokio::net"#;
        let _bytes = br#"reqwest::get"#;
        Ok(())
    }
}
'''
        self.assertEqual(codes(src), [])

    def test_imports_of_external_effect_apis_are_flagged_on_the_use_line(self) -> None:
        for stmt in (
            "use reqwest::Client;",
            "use reqwest::Client as _;",
            "use std::net::TcpStream;",
            "use std::net::{TcpStream, UdpSocket};",
            "use std::{io, net::TcpStream};",
            "use tokio::net as n;",
            "use tokio::{net::TcpStream, io};",
            "pub use spot_mail::Mailer;",
            "use lettre::Transport;",
            "use spot_egress::{Egress, Request};",
            "use spot_webhooks::fire;",
            "use std::\n    net::TcpStream;",
        ):
            with self.subTest(stmt=stmt):
                src = HEADER + stmt + "\nimpl Ctl { fn x(&self) {} }\n"
                found = check.scan_source("corpus.rs", src)
                self.assertEqual([v.code for v in found], [check.CODE_DIRECT])
                self.assertEqual(found[0].line, src.split("\n").index(stmt.split("\n")[0]) + 1)

    def test_an_imported_name_used_later_is_caught_at_the_import(self) -> None:
        src = HEADER + "use reqwest::Client;\nimpl Ctl { fn x(&self) { let _c = Client::new(); } }\n"
        self.assertEqual(codes(src), [check.CODE_DIRECT])

    def test_an_allowed_import_is_excused_with_a_reason(self) -> None:
        src = HEADER + "// effects-allow: type only, the client is built in the committed phase\nuse reqwest::Client;\n"
        self.assertEqual(codes(src), [])

    def test_unrelated_imports_and_lookalike_paths_pass(self) -> None:
        src = HEADER + "use std::collections::HashMap;\nuse std::sync::Arc;\nuse spot_app_api::net_policy;\n"
        self.assertEqual(codes(src), [])

    def test_the_line_number_is_the_offending_line(self) -> None:
        src = HEADER + "impl Ctl {\n    async fn notify(&self) {\n        let a = 1;\n        reqwest::get(u);\n    }\n}\n"
        self.assertEqual(lines(src), [src.split("\n").index("        reqwest::get(u);") + 1])

    def test_a_test_module_inside_a_controller_file_is_skipped(self) -> None:
        src = HEADER + "#[cfg(test)]\nmod tests {\n    fn t() { std::net::TcpListener::bind(a); }\n}\n"
        self.assertEqual(codes(src), [])

    def test_a_forbidden_type_in_a_field_is_flagged(self) -> None:
        src = HEADER + "struct Holder { client: reqwest::Client }\n"
        self.assertEqual(codes(src), [check.CODE_DIRECT])

    def test_a_step_that_is_not_committed_cannot_use_the_marker_escape(self) -> None:
        src = HEADER + '''
impl Ctl {
    // effect: committed-phase
    async fn notify(&self, _c: &mut dyn HookContext) -> AppResult<()> { reqwest::get(u); Ok(()) }
    async fn before_post_check(&self, c: &mut dyn HookContext) -> AppResult<()> { self.notify(c).await }
}
'''
        # `notify` is called from `before_post_check` and from the committed match arm; the first
        # call is not committed, so the function is not accepted.
        self.assertEqual(sorted(set(codes(src))), sorted({check.CODE_DIRECT, check.CODE_COMMITTED}))


class CommittedPhase(unittest.TestCase):
    def test_an_adjacent_committed_arm_does_not_cover_a_non_committed_arm(self) -> None:
        src = """
impl Controller for Ctl {
    fn run_step(&self, step: &StepName) {
        match step.as_str() {
            "shop:a_committed" => Box::pin(self.a()),
            "shop:b" => Box::pin(self.deliver()),
            _ => Box::pin(self.a()),
        }
    }
}
impl Ctl {
    fn a(&self) {}
    // effect: committed-phase
    fn deliver(&self) { lettre::send(m); }
}
"""
        self.assertEqual(sorted(set(codes(src))), sorted({check.CODE_DIRECT, check.CODE_COMMITTED}))

    def test_a_committed_arm_with_a_block_body_is_accepted(self) -> None:
        src = """
impl Controller for Ctl {
    fn run_step(&self, step: &StepName) {
        match step.as_str() {
            "shop:b" => { self.a(); }
            "shop:a_committed" => {
                let r = self.prepare();
                if r { self.deliver(); }
            }
            _ => {}
        }
    }
}
impl Ctl {
    // effect: committed-phase
    fn deliver(&self) { lettre::send(m); }
}
"""
        self.assertEqual(codes(src), [])

    def test_an_arm_mixing_committed_and_other_patterns_is_not_committed(self) -> None:
        src = """
impl Controller for Ctl {
    fn run_step(&self, step: &StepName) {
        match step.as_str() {
            "shop:a_committed" | "shop:b" => self.deliver(),
            _ => {}
        }
    }
}
impl Ctl {
    // effect: committed-phase
    fn deliver(&self) { lettre::send(m); }
}
"""
        self.assertEqual(sorted(set(codes(src))), sorted({check.CODE_DIRECT, check.CODE_COMMITTED}))

    def test_a_committed_literal_outside_an_arm_does_not_count(self) -> None:
        src = """
impl Controller for Ctl {
    fn run_step(&self, step: &StepName) {
        let _name = "shop:a_committed";
        self.deliver();
    }
}
impl Ctl {
    // effect: committed-phase
    fn deliver(&self) { lettre::send(m); }
}
"""
        self.assertEqual(sorted(set(codes(src))), sorted({check.CODE_DIRECT, check.CODE_COMMITTED}))

    def test_marked_function_called_from_a_committed_arm_is_accepted(self) -> None:
        src = HEADER + '''
impl Ctl {
    // effect: committed-phase
    async fn notify(&self, _c: &mut dyn HookContext) -> AppResult<()> { reqwest::get(u); Ok(()) }
}
'''
        self.assertEqual(codes(src), [])

    def test_marker_survives_attributes_between_comment_and_fn(self) -> None:
        src = HEADER + '''
impl Ctl {
    // effect: committed-phase
    #[allow(clippy::unused_async)]
    async fn notify(&self, _c: &mut dyn HookContext) -> AppResult<()> { reqwest::get(u); Ok(()) }
}
'''
        self.assertEqual(codes(src), [])

    def test_called_from_a_function_named_committed_is_accepted(self) -> None:
        src = '''
impl Controller for Ctl {
    fn run_step(&self) { self.post_committed(); }
}
impl Ctl {
    fn post_committed(&self) { self.send(); }
    // effect: committed-phase
    fn send(&self) { spot_egress::post(x); }
}
'''
        self.assertEqual(codes(src), [])

    def test_reachability_is_transitive_through_marked_helpers(self) -> None:
        src = '''
impl Controller for Ctl {}
impl Ctl {
    fn post_committed(&self) { self.outer(); }
    // effect: committed-phase
    fn outer(&self) { self.inner(); }
    // effect: committed-phase
    fn inner(&self) { lettre::send(m); }
}
'''
        self.assertEqual(codes(src), [])

    def test_called_from_a_non_committed_step_is_rejected(self) -> None:
        src = '''
impl Controller for Ctl {}
impl Ctl {
    fn after_post_mail(&self) { self.send(); }
    // effect: committed-phase
    fn send(&self) { lettre::send(m); }
}
'''
        self.assertEqual(sorted(set(codes(src))), sorted({check.CODE_DIRECT, check.CODE_COMMITTED}))

    def test_marked_but_never_called_is_rejected(self) -> None:
        src = '''
impl Controller for Ctl {}
impl Ctl {
    // effect: committed-phase
    fn send(&self) { lettre::send(m); }
}
'''
        self.assertEqual(sorted(set(codes(src))), sorted({check.CODE_DIRECT, check.CODE_COMMITTED}))

    def test_marker_on_a_different_function_does_not_cover_the_next_one(self) -> None:
        src = '''
impl Controller for Ctl {}
impl Ctl {
    fn post_committed(&self) { self.a(); self.b(); }
    // effect: committed-phase
    fn a(&self) { lettre::send(m); }
    fn b(&self) { lettre::send(m); }
}
'''
        self.assertEqual(codes(src), [check.CODE_DIRECT])
        self.assertEqual(lines(src), [7])


class Escapes(unittest.TestCase):
    def test_allow_with_a_reason_on_the_same_or_previous_line(self) -> None:
        src = HEADER + '''
impl Ctl {
    async fn notify(&self) {
        reqwest::get(u); // effects-allow: loopback health probe, no external effect
        // effects-allow: reading the mail template from the local mock
        spot_mail::template();
    }
}
'''
        self.assertEqual(codes(src), [])

    def test_a_trailing_allow_on_a_code_line_does_not_excuse_the_next_line(self) -> None:
        src = HEADER + """
impl Ctl {
    async fn notify(&self) {
        let a = 1; // effects-allow: this line only
        reqwest::get(u);
    }
}
"""
        self.assertEqual(codes(src), [check.CODE_DIRECT])

    def test_allow_without_a_reason_does_not_excuse(self) -> None:
        src = HEADER + "impl Ctl {\n    async fn notify(&self) {\n        reqwest::get(u); // effects-allow:\n    }\n}\n"
        self.assertEqual(sorted(set(codes(src))), sorted({check.CODE_DIRECT, check.CODE_ALLOW_REASON}))


class Files(unittest.TestCase):
    def test_run_scans_directories_and_skips_target(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / "src").mkdir()
            (root / "target").mkdir()
            bad = "impl Controller for C {}\nfn f() { reqwest::get(u); }\n"
            (root / "src" / "bad.rs").write_text(bad)
            (root / "target" / "bad.rs").write_text(bad)
            found = check.run([root], root)
            self.assertEqual([(v.path, v.code) for v in found], [("src/bad.rs", check.CODE_DIRECT)])

    def test_a_root_nested_under_a_skipped_directory_name_is_still_scanned(self) -> None:
        bad = "impl Controller for C {}\nfn f() { reqwest::get(u); }\n"
        for outer in ("target", "worktrees", ".git", "node_modules"):
            with self.subTest(outer=outer), tempfile.TemporaryDirectory() as tmp:
                root = Path(tmp) / outer / "checkout"
                (root / "src").mkdir(parents=True)
                (root / "src" / "bad.rs").write_text(bad)
                (root / "target").mkdir()
                (root / "target" / "bad.rs").write_text(bad)
                found = check.run([root], root)
                self.assertEqual([(v.path, v.code) for v in found], [("src/bad.rs", check.CODE_DIRECT)])

    def test_a_named_path_that_does_not_exist_exits_2(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            self.assertEqual(check.main([str(Path(tmp) / "typo")]), 2)

    def test_an_existing_clean_path_exits_0_and_a_bad_one_exits_1(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            good = Path(tmp) / "good.rs"
            good.write_text("fn f() {}\n")
            bad = Path(tmp) / "bad.rs"
            bad.write_text("impl Controller for C {}\nfn f() { reqwest::get(u); }\n")
            self.assertEqual(check.main([str(good)]), 0)
            self.assertEqual(check.main([str(bad)]), 1)

    def test_the_repository_itself_is_clean(self) -> None:
        repo = HERE.parents[3]
        found = check.run([repo / "crates", repo / "apps"], repo)
        self.assertEqual([str(v) for v in found], [])


if __name__ == "__main__":
    unittest.main()
