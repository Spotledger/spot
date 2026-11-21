//! `spot mgmt` against `spot-mgmt-api::router` served over mutual TLS on an ephemeral local port,
//! with certificates made by `spot mgmt ca init` itself.
#![allow(
    missing_docs,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    dead_code,
    unused_imports
)]

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use clap::Command;
use rustls::pki_types::CertificateDer;
use rustls::pki_types::PrivateKeyDer;
use rustls::pki_types::pem::PemObject as _;
use spot_core::{Clock, ManualClock};
use spot_mgmt_api::lifecycle::MemoryCommandStore;
use spot_mgmt_api::ports::{MemoryAuditSink, NoApprovals, OperatorAllowList, OperatorEntry};
use spot_mgmt_api::stubs::{stub_catalogue, stub_handlers};
use spot_mgmt_api::tls::{HandshakeLimiter, serve};
use spot_mgmt_api::{Deps, router, tls_config};
use spot_signed_command::MemoryNonceStore;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[path = "../src/commands/mod.rs"]
mod commands;

use commands::{CommandCtx, Exit};

const SITE: &str = "site-1";

#[derive(Clone, Default)]
struct Sink(Arc<Mutex<Vec<u8>>>);

impl Write for Sink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Sink {
    fn text(&self) -> String {
        String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
    }
}

/// The wall clock, fixed at the first call: the management server's certificate is issued at this
/// instant and the client verifies it against the real clock, so a made-up date would make it
/// "not valid yet" (or expired).
fn clock() -> Arc<dyn Clock> {
    static START: std::sync::OnceLock<time::OffsetDateTime> = std::sync::OnceLock::new();
    let start = *START.get_or_init(time::OffsetDateTime::now_utc);
    Arc::new(ManualClock::new(start))
}

/// Runs `spot mgmt <args>`; returns the exit, standard output and standard error.
async fn mgmt(args: &[&str]) -> (Exit, String, String) {
    let (out, err) = (Sink::default(), Sink::default());
    let mut ctx = CommandCtx::new(
        PathBuf::from("/nonexistent"),
        clock(),
        Box::new(out.clone()),
        Box::new(err.clone()),
    );
    let mut argv = vec!["mgmt"];
    argv.extend_from_slice(args);
    let matches = match commands::mgmt::command().try_get_matches_from(argv) {
        Ok(matches) => matches,
        Err(error) => return (Exit::Usage, String::new(), error.to_string()),
    };
    let exit = commands::mgmt::run(&matches, &mut ctx).await.unwrap();
    (exit, out.text(), err.text())
}

struct Site {
    dir: PathBuf,
    address: String,
    key_file: PathBuf,
}

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("spot-mgmt-test-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn public_key_of(stdout: &str) -> [u8; 32] {
    let line = stdout
        .lines()
        .find(|l| l.starts_with("public_key"))
        .unwrap();
    let hex = line.split('"').nth(1).unwrap();
    let mut out = [0u8; 32];
    for (index, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[index * 2..index * 2 + 2], 16).unwrap();
    }
    out
}

/// Makes the trust material with the command, then serves the management API with it.
async fn site(tag: &str) -> Site {
    let dir = temp_dir(tag);
    let d = dir.to_str().unwrap();
    let (exit, _, err) = mgmt(&["ca", "init", "--dir", d, "--server-name", "127.0.0.1"]).await;
    assert_eq!(exit, Exit::Ok, "{err}");
    let key_file = dir.join("admin.key");
    let (exit, out, _) = mgmt(&["key", "gen", "--out", key_file.to_str().unwrap()]).await;
    assert_eq!(exit, Exit::Ok);
    let admin = public_key_of(&out);

    let ca = CertificateDer::from_pem_file(dir.join("ca.pem")).unwrap();
    let chain: Vec<_> = CertificateDer::pem_file_iter(dir.join("server.pem"))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    let server_key = PrivateKeyDer::from_pem_file(dir.join("server.key")).unwrap();
    let config = tls_config(chain, server_key, std::slice::from_ref(&ca)).unwrap();
    let deps = Deps {
        catalogue: Arc::new(stub_catalogue().unwrap()),
        handlers: stub_handlers(),
        nonces: Arc::new(MemoryNonceStore::new()),
        administrator_keys: vec![admin],
        operator_ca: vec![ca],
        operators: OperatorAllowList::new(vec![OperatorEntry::new("CN=operator", "operator")]),
        approvals: Arc::new(NoApprovals),
        clock: clock(),
        audit: Arc::new(MemoryAuditSink::new()),
        commands: Arc::new(MemoryCommandStore::new()),
        site_id: SITE.to_owned(),
    };
    let app = router(deps);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap().to_string();
    tokio::spawn(async move {
        let _ = serve(
            listener,
            Arc::new(config),
            app,
            clock(),
            Arc::new(HandshakeLimiter::standard()),
            std::future::pending::<()>(),
        )
        .await;
    });
    Site {
        dir,
        address,
        key_file,
    }
}

impl Site {
    fn remote(&self) -> Vec<String> {
        let d = |name: &str| self.dir.join(name).to_str().unwrap().to_owned();
        vec![
            "--url".into(),
            format!("https://{}", self.address),
            "--cert".into(),
            d("client.pem"),
            "--key".into(),
            d("client.key"),
            "--ca".into(),
            d("ca.pem"),
        ]
    }

    async fn run(&self, verb: &str, extra: &[&str]) -> (Exit, String, String) {
        let mut args: Vec<String> = vec![verb.to_owned()];
        args.extend(extra.iter().map(|s| (*s).to_owned()));
        args.extend(self.remote());
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        mgmt(&refs).await
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_describe_validate_run_and_status_round_trip() {
    let s = site("roundtrip").await;
    let (exit, out, err) = s.run("list", &[]).await;
    assert_eq!(exit, Exit::Ok, "{err}");
    assert!(out.contains("site.backup.run"));
    let (exit, out, _) = s.run("describe", &["site.backup.run"]).await;
    assert_eq!(exit, Exit::Ok);
    assert!(out.contains("--param target"));
    let (exit, out, err) = s
        .run(
            "validate",
            &[
                "site.backup.run",
                "--param",
                "site=site-1",
                "--param",
                "target=local",
            ],
        )
        .await;
    assert_eq!(exit, Exit::Ok, "{err}");
    assert!(out.contains("backup"), "the equivalent command line: {out}");
    let key = s.key_file.to_str().unwrap();
    let (exit, out, err) = s
        .run(
            "run",
            &[
                "site.backup.run",
                "--param",
                "site=site-1",
                "--param",
                "target=local",
                "--audience",
                SITE,
                "--sign-with",
                key,
                "--wait",
            ],
        )
        .await;
    assert_eq!(exit, Exit::Ok, "{err}");
    assert!(out.contains("succeeded"), "{out}");
    let id = out.split_whitespace().next().unwrap();
    let (exit, out, _) = s.run("status", &[id]).await;
    assert_eq!(exit, Exit::Ok);
    assert!(out.contains("succeeded"));
    let (exit, out, _) = s.run("status", &["--json", id]).await;
    assert_eq!(exit, Exit::Ok);
    assert!(serde_json::from_str::<serde_json::Value>(&out).is_ok());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_missing_parameter_reports_which() {
    let s = site("missing").await;
    let key = s.key_file.to_str().unwrap();
    let (exit, _, err) = s
        .run(
            "run",
            &[
                "site.backup.run",
                "--param",
                "site=site-1",
                "--audience",
                SITE,
                "--sign-with",
                key,
            ],
        )
        .await;
    assert_eq!(exit, Exit::Failed);
    assert!(err.contains("parameter `target`"), "{err}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_secret_is_never_a_param_value_and_comes_from_a_file() {
    let s = site("secret").await;
    let key = s.key_file.to_str().unwrap();
    let (exit, _, err) = s
        .run(
            "run",
            &[
                "site.restore.apply",
                "--param",
                "site=site-1",
                "--param",
                "confirm=TOKEN12345678",
                "--audience",
                SITE,
                "--sign-with",
                key,
            ],
        )
        .await;
    assert_eq!(exit, Exit::Refused);
    assert!(err.contains("--param-file"), "{err}");
    assert!(!err.contains("TOKEN12345678"));
    let secret = s.dir.join("confirm.txt");
    std::fs::write(&secret, "TOKEN12345678\n").unwrap();
    let pair = format!("confirm={}", secret.display());
    let (exit, out, err) = s
        .run(
            "run",
            &[
                "site.restore.apply",
                "--param",
                "site=site-1",
                "--param-file",
                &pair,
                "--audience",
                SITE,
                "--sign-with",
                key,
                "--wait",
            ],
        )
        .await;
    assert_eq!(exit, Exit::Ok, "{err}");
    assert!(out.contains("succeeded"));
    assert!(!out.contains("TOKEN12345678"));
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn key_files_with_loose_permissions_are_refused_and_files_are_private() {
    use std::os::unix::fs::PermissionsExt as _;
    let s = site("perms").await;
    for name in ["ca.key", "client.key", "server.key", "admin.key"] {
        let mode = std::fs::metadata(s.dir.join(name))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "{name}");
    }
    let key = s.key_file.clone();
    std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o644)).unwrap();
    let (exit, _, err) = s
        .run(
            "run",
            &[
                "site.backup.run",
                "--param",
                "site=site-1",
                "--param",
                "target=local",
                "--audience",
                SITE,
                "--sign-with",
                key.to_str().unwrap(),
            ],
        )
        .await;
    assert_eq!(exit, Exit::Refused);
    assert!(err.contains("chmod 600"), "{err}");
    // `key gen` never overwrites.
    let (exit, _, _) = mgmt(&["key", "gen", "--out", key.to_str().unwrap()]).await;
    assert_eq!(exit, Exit::Failed);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_signed_operation_without_a_key_is_refused_and_a_read_one_is_not() {
    let s = site("unsigned").await;
    let (exit, _, err) = s
        .run(
            "run",
            &[
                "site.backup.run",
                "--param",
                "site=site-1",
                "--param",
                "target=local",
                "--audience",
                SITE,
            ],
        )
        .await;
    assert_eq!(exit, Exit::Refused);
    assert!(err.contains("--sign-with"), "{err}");
    let (exit, out, err) = s
        .run(
            "run",
            &[
                "site.status",
                "--param",
                "site=site-1",
                "--audience",
                SITE,
                "--wait",
            ],
        )
        .await;
    assert_eq!(exit, Exit::Ok, "{err}");
    assert!(out.contains("succeeded"));
}

/// POSTs a body to the commands endpoint with the CLI's own client certificate and returns the
/// status code (a minimal client for the replay test).
async fn post_raw(s: &Site, body: &str, idempotency: &str) -> u16 {
    let dir = &s.dir;
    let mut roots = rustls::RootCertStore::empty();
    roots
        .add(CertificateDer::from_pem_file(dir.join("ca.pem")).unwrap())
        .unwrap();
    let chain: Vec<_> = CertificateDer::pem_file_iter(dir.join("client.pem"))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    let key = PrivateKeyDer::from_pem_file(dir.join("client.key")).unwrap();
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_protocol_versions(&[&rustls::version::TLS13])
    .unwrap()
    .with_root_certificates(roots)
    .with_client_auth_cert(chain, key)
    .unwrap();
    let tcp = tokio::net::TcpStream::connect(&s.address).await.unwrap();
    let name = rustls::pki_types::ServerName::try_from("127.0.0.1".to_owned()).unwrap();
    let mut tls = tokio_rustls::TlsConnector::from(Arc::new(config))
        .connect(name, tcp)
        .await
        .unwrap();
    let request = format!(
        "POST /mgmt/v1/commands HTTP/1.1\r\nHost: x\r\nConnection: close\r\nIdempotency-Key: {idempotency}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    tls.write_all(request.as_bytes()).await.unwrap();
    let mut raw = Vec::new();
    tls.read_to_end(&mut raw).await.unwrap();
    let text = String::from_utf8_lossy(&raw).into_owned();
    text.split_whitespace().nth(1).unwrap().parse().unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_replay_of_the_same_envelope_is_refused() {
    let s = site("replay").await;
    let out_file = s.dir.join("envelope.json");
    let (exit, _, err) = mgmt(&[
        "key",
        "sign",
        "site.backup.run",
        "--key-file",
        s.key_file.to_str().unwrap(),
        "--audience",
        SITE,
        "--param",
        "site=site-1",
        "--param",
        "target=local",
        "--out",
        out_file.to_str().unwrap(),
    ])
    .await;
    assert_eq!(exit, Exit::Ok, "{err}");
    let envelope = std::fs::read_to_string(&out_file).unwrap();
    assert_eq!(post_raw(&s, &envelope, "first").await, 202);
    assert_eq!(post_raw(&s, &envelope, "second").await, 409);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn there_is_no_insecure_flag_and_plain_http_is_refused() {
    let help = commands::mgmt::command().render_long_help().to_string();
    assert!(!help.to_lowercase().contains("insecure"));
    for verb in ["list", "describe", "validate", "run", "status", "cancel"] {
        let mut cmd: Command = commands::mgmt::command();
        let sub = cmd.find_subcommand_mut(verb).unwrap();
        assert!(
            sub.render_long_help()
                .to_string()
                .to_lowercase()
                .contains("--ca")
        );
        assert!(
            !sub.render_long_help()
                .to_string()
                .to_lowercase()
                .contains("insecure")
        );
    }
    let (exit, _, err) = mgmt(&[
        "list",
        "--url",
        "http://localhost:9443",
        "--cert",
        "c",
        "--key",
        "k",
        "--ca",
        "a",
    ])
    .await;
    assert_eq!(exit, Exit::Usage, "{err}");
    let (exit, _, _) = mgmt(&[
        "list",
        "--insecure",
        "--url",
        "https://x",
        "--cert",
        "c",
        "--key",
        "k",
        "--ca",
        "a",
    ])
    .await;
    assert_eq!(exit, Exit::Usage);
}
