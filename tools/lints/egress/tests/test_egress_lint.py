"""Tests for the egress lint: throw-away trees, each with the verdict it must get."""

from __future__ import annotations

import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
LINT_DIR = HERE.parent
REPO = LINT_DIR.parent.parent.parent
sys.path.insert(0, str(LINT_DIR))
import egress_lint  # noqa: E402

PRIMITIVES = {
    "tcp_connect": "fn f() { let _ = std::net::TcpStream::connect(\"x:1\"); }",
    "tokio_tcp": "async fn f() { let _ = tokio::net::TcpStream::connect(\"x:1\").await; }",
    "udp_connect": "fn f() { let _ = UdpSocket::connect(&s, \"x:1\"); }",
    "to_socket_addrs": "use std::net::ToSocketAddrs;\nfn f() { let a = \"x:1\".to_socket_addrs(); let _ = connect(a); }",
    "hyper_util_client": "fn f() { let _c = hyper_util::client::legacy::Client::builder(); }",
    "hyper_client": "fn f() { let _c = hyper::client::conn::http1::handshake; }",
    "reqwest": "fn f() { let _ = reqwest::get(\"u\"); }",
    "ureq": "fn f() { let _ = ureq::get(\"u\"); }",
    "isahc": "fn f() { let _ = isahc::get(\"u\"); }",
    "surf": "fn f() { let _ = surf::get(\"u\"); }",
    "curl": "fn f() { let _ = curl::easy::Easy::new(); }",
    "lettre": "fn f() { let _ = lettre::SmtpTransport::unencrypted_localhost(); }",
    "lettre_transport_path": "fn f() { let _ = lettre::transport::smtp::SmtpTransport::relay(\"h\"); }",
    "tls_connector": "fn f() { let _ = tokio_rustls::TlsConnector::from(c); }",
    "native_tls": "fn f() { let _ = native_tls::TlsConnector::new(); }",
    "tokio_native_tls": "fn f() { let _ = tokio_native_tls::TlsConnector::from(c); }",
    "hyper_rustls": "fn f() { let _ = hyper_rustls::HttpsConnectorBuilder::new(); }",
    "client_connection": "fn f() { let _ = rustls::ClientConnection::new(cfg, name); }",
}

IMPORTS = {
    "use_tokio_tcp": "use tokio::net::TcpStream;\n",
    "use_group": "use tokio::net::{TcpListener, TcpStream};\n",
    "use_alias": "use tokio::net::TcpStream as S;\n",
    "use_reqwest": "use reqwest::Client;\n",
    "use_lettre_transport": "use lettre::transport::smtp::SmtpTransport;\n",
    "use_lettre_group": "use lettre::{message::Mailbox, AsyncSmtpTransport};\n",
    "use_hyper_client": "use hyper::client::conn;\n",
    "use_hyper_util_client": "use hyper_util::client::legacy::Client;\n",
    "use_tokio_rustls": "use tokio_rustls::{TlsAcceptor, TlsConnector};\n",
    "use_rustls_cc": "use rustls::ClientConnection;\n",
    "pub_use": "pub(crate) use ureq::Agent;\n",
}


def write(root: Path, rel: str, text: str) -> None:
    path = root / rel
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(text, encoding="utf-8")


def cargo(deps: str = "", dev: str = "") -> str:
    return f'[package]\nname = "x"\nversion = "0.0.0"\n\n[dependencies]\n{deps}\n\n[dev-dependencies]\n{dev}\n'


def entry(path: str, kind: str = "egress-implementation", reason: str = "because", **extra: str) -> str:
    lines = ["[[allow]]", f'path = "{path}"', f'kind = "{kind}"', 'owner_wp = "WP-X"', f'reason = "{reason}"']
    lines += [f'{k} = "{v}"' for k, v in extra.items()]
    return "\n".join(lines) + "\n"


class Tree:
    def __init__(self) -> None:
        self._tmp = tempfile.TemporaryDirectory()
        self.root = Path(self._tmp.name)

    def close(self) -> None:
        self._tmp.cleanup()

    def allow(self, text: str) -> None:
        write(self.root, "tools/lints/egress/allow.toml", text)

    def run(self) -> tuple[int, list[str]]:
        return egress_lint.run(self.root)


class LintTests(unittest.TestCase):
    def setUp(self) -> None:
        self.t = Tree()
        self.addCleanup(self.t.close)

    def src(self, text: str, rel: str = "crates/framework/x/src/lib.rs") -> None:
        write(self.t.root, rel, text)

    def test_clean_tree_passes(self) -> None:
        self.src("pub fn add(a: u32, b: u32) -> u32 { a + b }\n")
        status, lines = self.t.run()
        self.assertEqual(status, 0, lines)

    def test_each_primitive_is_caught_with_file_and_line(self) -> None:
        for name, text in PRIMITIVES.items():
            with self.subTest(name):
                self.src("// header\n" + text + "\n")
                status, lines = self.t.run()
                self.assertEqual(status, 1, (name, lines))
                self.assertTrue(
                    any(ln.startswith("crates/framework/x/src/lib.rs:") and egress_lint.CODE_DIRECT in ln for ln in lines),
                    (name, lines),
                )

    def test_each_import_is_caught(self) -> None:
        for name, text in IMPORTS.items():
            with self.subTest(name):
                self.src(text + "pub fn f() {}\n")
                status, lines = self.t.run()
                self.assertEqual(status, 1, (name, lines))
                self.assertTrue(any(ln.startswith("crates/framework/x/src/lib.rs:1:") for ln in lines), (name, lines))

    def test_import_only_hit_is_reported_once_at_the_use_line(self) -> None:
        self.src("\nuse tokio::net::TcpStream as S;\npub fn f(_s: Option<S>) {}\n")
        status, lines = self.t.run()
        self.assertEqual(status, 1)
        self.assertEqual([ln for ln in lines if egress_lint.CODE_DIRECT in ln].__len__(), 1, lines)
        self.assertIn("lib.rs:2:", "\n".join(lines))

    def test_lettre_message_building_is_not_a_finding(self) -> None:
        self.src(
            "use lettre::message::header::ContentType;\nuse lettre::Address;\n"
            "fn f() -> lettre::message::Mailbox { lettre::Message::builder(); todo() }\n"
        )
        status, lines = self.t.run()
        self.assertEqual(status, 0, lines)

    def test_comment_string_cfg_test_and_tests_dir_are_ignored(self) -> None:
        self.src(
            "// TcpStream::connect(x) and reqwest::get(u)\n"
            "/* ureq::get(u) /* nested lettre::SmtpTransport */ */\n"
            'const A: &str = "tokio::net::TcpStream::connect reqwest";\n'
            'const B: &str = r#"hyper_util::client native_tls"#;\n'
            "#[cfg(test)]\nmod tests {\n    use tokio::net::TcpStream;\n"
            "    fn t() { let _ = TcpStream::connect(\"x\"); let _ = reqwest::get(\"u\"); }\n}\n"
            "pub fn ok() {}\n"
        )
        self.src("use tokio::net::TcpStream;\nfn t() { let _ = reqwest::get(\"u\"); }\n", "crates/framework/x/tests/net.rs")
        self.src("use ureq::Agent;\n", "crates/framework/x/benches/b.rs")
        self.src("use ureq::Agent;\n", "crates/framework/x/target/debug/build.rs")
        status, lines = self.t.run()
        self.assertEqual(status, 0, lines)

    def test_code_after_cfg_test_module_is_scanned(self) -> None:
        self.src("#[cfg(test)]\nmod tests { fn t() {} }\nfn f() { let _ = reqwest::get(\"u\"); }\n")
        status, lines = self.t.run()
        self.assertEqual(status, 1)
        self.assertIn("lib.rs:3:", "\n".join(lines))

    def test_scans_apps_and_harness_roots(self) -> None:
        self.src("fn f() { let _ = ureq::get(\"u\"); }\n", "apps/a/src/lib.rs")
        self.src("fn f() { let _ = ureq::get(\"u\"); }\n", "harness/h/src/lib.rs")
        status, lines = self.t.run()
        self.assertEqual(status, 1)
        text = "\n".join(lines)
        self.assertIn("apps/a/src/lib.rs:1:", text)
        self.assertIn("harness/h/src/lib.rs:1:", text)

    def test_dependency_in_dependencies_is_reported_and_dev_is_not(self) -> None:
        write(self.t.root, "crates/framework/x/Cargo.toml", cargo(deps='reqwest = "0.12"'))
        status, lines = self.t.run()
        self.assertEqual(status, 1)
        self.assertTrue(any("Cargo.toml:6:" in ln and egress_lint.CODE_DEP in ln for ln in lines), lines)
        write(self.t.root, "crates/framework/x/Cargo.toml", cargo(dev='reqwest = "0.12"'))
        status, lines = self.t.run()
        self.assertEqual(status, 0, lines)

    def test_dependency_spellings_are_caught(self) -> None:
        for deps in (
            'native-tls = "0.2"',
            'tokio-native-tls = "0.3"',
            'hyper-rustls = "0.27"',
            'lettre = { version = "0.11", default-features = false }',
            'mailer = { package = "lettre", version = "0.11" }',
            'ureq = { version = "2" }',
            '[dependencies.isahc]\nversion = "1"',
        ):
            with self.subTest(deps):
                text = cargo(deps=deps) if not deps.startswith("[") else cargo() + deps + "\n"
                write(self.t.root, "crates/framework/x/Cargo.toml", text)
                status, lines = self.t.run()
                self.assertEqual(status, 1, lines)

    def test_target_specific_dependency_is_caught(self) -> None:
        write(
            self.t.root,
            "crates/framework/x/Cargo.toml",
            cargo() + '\n[target.\'cfg(windows)\'.dependencies]\nsurf = "2"\n',
        )
        status, _ = self.t.run()
        self.assertEqual(status, 1)

    def test_allow_entry_silences_exactly_its_file(self) -> None:
        bad = "fn f() { let _ = ureq::get(\"u\"); }\n"
        self.src(bad, "crates/framework/a/src/lib.rs")
        self.src(bad, "crates/framework/b/src/lib.rs")
        self.t.allow(entry("crates/framework/a/src/lib.rs"))
        status, lines = self.t.run()
        self.assertEqual(status, 1)
        text = "\n".join(lines)
        self.assertIn("crates/framework/b/src/lib.rs:1:", text)
        self.assertNotIn("crates/framework/a/src/lib.rs:1:", text)

    def test_allow_entry_silences_a_cargo_file(self) -> None:
        write(self.t.root, "crates/framework/x/Cargo.toml", cargo(deps='ureq = "2"'))
        self.t.allow(entry("crates/framework/x/Cargo.toml"))
        status, lines = self.t.run()
        self.assertEqual(status, 0, lines)

    def test_stale_entry_fails(self) -> None:
        self.src("pub fn ok() {}\n")
        self.t.allow(entry("crates/framework/x/src/lib.rs"))
        status, lines = self.t.run()
        self.assertEqual(status, 1)
        self.assertTrue(any(egress_lint.CODE_STALE in ln for ln in lines), lines)

    def test_entry_without_reason_fails(self) -> None:
        self.src("fn f() { let _ = ureq::get(\"u\"); }\n")
        for reason in ("", "   "):
            with self.subTest(reason):
                self.t.allow(entry("crates/framework/x/src/lib.rs", reason=reason))
                status, lines = self.t.run()
                self.assertEqual(status, 1)
                self.assertTrue(any(egress_lint.CODE_REASON in ln for ln in lines), lines)
                # An entry that is rejected does not silence the hit either.
                self.assertTrue(any(egress_lint.CODE_DIRECT in ln for ln in lines), lines)

    def test_kind_specific_fields_and_unknown_kind_fail(self) -> None:
        self.src("fn f() { let _ = ureq::get(\"u\"); }\n")
        cases = [
            entry("crates/framework/x/src/lib.rs", kind="authorised-by-guard"),
            entry("crates/framework/x/src/lib.rs", kind="peer-pinned"),
            entry("crates/framework/x/src/lib.rs", kind="gap"),
            entry("crates/framework/x/src/lib.rs", kind="whatever"),
        ]
        for text in cases:
            with self.subTest(text):
                self.t.allow(text)
                status, lines = self.t.run()
                self.assertEqual(status, 1, lines)
                self.assertTrue(any(egress_lint.CODE_INVALID in ln for ln in lines), lines)

    def test_kind_specific_fields_accepted(self) -> None:
        self.src("fn f() { let _ = ureq::get(\"u\"); }\n")
        for text in (
            entry("crates/framework/x/src/lib.rs", kind="authorised-by-guard", function="send"),
            entry("crates/framework/x/src/lib.rs", kind="peer-pinned", pinned_at="tls.rs"),
            entry("crates/framework/x/src/lib.rs", kind="loopback-only"),
            entry("crates/framework/x/src/lib.rs", kind="harness"),
            "max_gaps = 1\n" + entry("crates/framework/x/src/lib.rs", kind="gap", tracked_by="WP-SEC-09.T6"),
        ):
            with self.subTest(text):
                self.t.allow(text)
                status, lines = self.t.run()
                self.assertEqual(status, 0, lines)

    def test_two_entries_for_one_file_fail(self) -> None:
        self.src("fn f() { let _ = ureq::get(\"u\"); }\n")
        self.t.allow(entry("crates/framework/x/src/lib.rs") + entry("crates/framework/x/src/lib.rs", kind="harness"))
        status, lines = self.t.run()
        self.assertEqual(status, 1)
        self.assertTrue(any(egress_lint.CODE_DUPLICATE in ln for ln in lines), lines)

    def test_gap_count_is_printed_and_growth_fails(self) -> None:
        self.src("fn f() { let _ = ureq::get(\"u\"); }\n", "crates/framework/a/src/lib.rs")
        self.src("fn f() { let _ = ureq::get(\"u\"); }\n", "crates/framework/b/src/lib.rs")
        gap = lambda p: entry(p, kind="gap", tracked_by="WP-SEC-09.T6")  # noqa: E731
        both = gap("crates/framework/a/src/lib.rs") + gap("crates/framework/b/src/lib.rs")
        self.t.allow("max_gaps = 2\n" + both)
        status, lines = self.t.run()
        self.assertEqual(status, 0, lines)
        self.assertIn("egress: gap entries: 2 (max_gaps = 2)", lines)
        self.t.allow("max_gaps = 1\n" + both)
        status, lines = self.t.run()
        self.assertEqual(status, 1)
        self.assertTrue(any(egress_lint.CODE_GAPS in ln for ln in lines), lines)
        self.assertIn("egress: gap entries: 2 (max_gaps = 1)", lines)

    def test_unreadable_allow_list_is_exit_2(self) -> None:
        self.src("pub fn ok() {}\n")
        self.t.allow("this is = not [toml\n")
        with self.assertRaises(egress_lint.InputError):
            self.t.run()
        proc = subprocess.run([sys.executable, str(LINT_DIR / "egress_lint.py"), str(self.t.root)], capture_output=True, text=True)
        self.assertEqual(proc.returncode, 2, proc.stderr)

    def test_missing_root_is_exit_2(self) -> None:
        proc = subprocess.run(
            [sys.executable, str(LINT_DIR / "egress_lint.py"), str(self.t.root / "nope")], capture_output=True, text=True
        )
        self.assertEqual(proc.returncode, 2, proc.stderr)

    def test_violation_exit_code_through_the_cli(self) -> None:
        self.src("fn f() { let _ = ureq::get(\"u\"); }\n")
        proc = subprocess.run([sys.executable, str(LINT_DIR / "egress_lint.py"), str(self.t.root)], capture_output=True, text=True)
        self.assertEqual(proc.returncode, 1)
        self.assertIn("crates/framework/x/src/lib.rs:1:", proc.stdout)


class RealRepositoryTests(unittest.TestCase):
    def test_real_repository_passes(self) -> None:
        proc = subprocess.run(["bash", str(LINT_DIR / "run.sh")], capture_output=True, text=True, cwd=REPO)
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        self.assertIn("egress: gap entries:", proc.stdout)


if __name__ == "__main__":
    unittest.main()
