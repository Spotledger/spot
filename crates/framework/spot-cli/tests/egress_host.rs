//! WP-SEC-09.T7: the egress policy is hosted once per site. The default policy, the tenant
//! allow-list, the denial file, the rate limit, a failing file, reload, the file mode and the
//! `spot egress` command. No network and no database: DNS is scripted, the clock is injected and
//! every file lives in a temporary directory.
#![allow(
    missing_docs,
    clippy::unwrap_used,
    clippy::expect_used,
    dead_code,
    unused_imports
)]

/// The command registry, driven without a process (the technique of `commands_registry.rs`).
#[path = "../src/commands/mod.rs"]
mod commands;

use std::collections::{BTreeMap, HashMap};
use std::io::Write;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use commands::{CommandCtx, Exit};
use spot_cli::services::egress::{
    self as svc, DenialLine, EgressHost, EgressStatus, Paths, Reloaded, Settings, codes,
};
use spot_cli::services::{Level, Service, SiteCtx};
use spot_core::{Clock, ManualClock};
use spot_egress::{AddrClass, EgressError, Guard, Purpose, Request, Resolver};
use time::macros::datetime;

// --- scaffolding -----------------------------------------------------------------------------

static COUNTER: AtomicU32 = AtomicU32::new(0);

struct Dir(PathBuf);

impl Dir {
    fn new(tag: &str) -> Self {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir =
            std::env::temp_dir().join(format!("spot_egress_host_{tag}_{}_{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Dir(dir)
    }
    fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn clock() -> Arc<ManualClock> {
    Arc::new(ManualClock::new(datetime!(2026-11-14 12:00:00 UTC)))
}

/// Answers from a fixed table; an unknown name does not resolve.
struct StaticResolver(HashMap<String, Vec<IpAddr>>);

#[async_trait::async_trait]
impl Resolver for StaticResolver {
    async fn resolve(&self, host: &str) -> Result<Vec<IpAddr>, std::io::Error> {
        self.0
            .get(host)
            .cloned()
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "no such host"))
    }
}

fn resolver(pairs: &[(&str, &str)]) -> Arc<StaticResolver> {
    Arc::new(StaticResolver(
        pairs
            .iter()
            .map(|(host, ip)| ((*host).to_owned(), vec![ip.parse().unwrap()]))
            .collect(),
    ))
}

fn world() -> Arc<StaticResolver> {
    resolver(&[
        ("127.0.0.1", "127.0.0.1"),
        ("10.0.0.1", "10.0.0.1"),
        ("169.254.169.254", "169.254.169.254"),
        ("::1", "::1"),
        ("[::1]", "::1"),
        ("rebind.test", "127.0.0.1"),
        ("public.test", "8.8.8.8"),
        ("api.example.test", "93.184.216.34"),
        ("other.example.test", "93.184.216.34"),
        ("internal.test", "10.0.0.1"),
    ])
}

fn settings(dir: &Dir, policy: Option<&str>, per_minute: u32) -> (Settings, Paths) {
    let policy_file = policy.map(|text| {
        let path = dir.path("policy.toml");
        std::fs::write(&path, text).unwrap();
        path
    });
    let settings = Settings {
        policy_file,
        denial_log: Some(dir.path("denials.log")),
        denial_log_max_bytes: None,
        denials_per_minute: Some(per_minute),
    };
    let paths = Paths::resolve(&settings, None, "acme", 1).unwrap();
    (settings, paths)
}

fn build(dir: &Dir, policy: Option<&str>, per_minute: u32) -> Arc<EgressHost> {
    let (settings, paths) = settings(dir, policy, per_minute);
    EgressHost::build(&settings, paths, clock(), world()).unwrap()
}

const ALLOW_API: &str = r#"
[tenants.acme]
[[tenants.acme.allow]]
scheme = "https"
host = "api.example.test"
ports = [443]
purpose = "webhook"
"#;

fn url(text: &str) -> url::Url {
    text.parse().unwrap()
}

fn req(text: &str, purpose: Purpose, tenant: &str) -> Request {
    Request {
        url: url(text),
        purpose,
        tenant: tenant.to_owned(),
        actor: Some("svc-actor".to_owned()),
    }
}

fn lines(path: &Path) -> Vec<String> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect()
}

// --- the default policy ----------------------------------------------------------------------

#[tokio::test]
async fn the_default_policy_refuses_what_must_not_be_reached() {
    let dir = Dir::new("default");
    let guard = build(&dir, None, 60).guard();
    for host in [
        "127.0.0.1",
        "10.0.0.1",
        "169.254.169.254",
        "::1",
        "rebind.test",
    ] {
        let denied = guard
            .authorize_addr(host, 80, Purpose::Webhook, "acme", None)
            .await;
        assert!(
            matches!(denied, Err(EgressError::AddressDenied { .. })),
            "{host}: {denied:?}"
        );
    }
    // The classes are the right ones.
    assert!(matches!(
        guard
            .authorize_addr("127.0.0.1", 80, Purpose::Webhook, "acme", None)
            .await,
        Err(EgressError::AddressDenied {
            class: AddrClass::Loopback
        })
    ));
    assert!(matches!(
        guard
            .authorize_addr("10.0.0.1", 80, Purpose::Webhook, "acme", None)
            .await,
        Err(EgressError::AddressDenied {
            class: AddrClass::Private
        })
    ));
    assert!(matches!(
        guard
            .authorize_addr("169.254.169.254", 80, Purpose::Webhook, "acme", None)
            .await,
        Err(EgressError::AddressDenied {
            class: AddrClass::CloudMetadata
        })
    ));
    // The URL form refuses an unlisted tenant outright, `[::1]` included.
    assert!(
        guard
            .authorize(&req("http://[::1]/", Purpose::Webhook, "acme"))
            .await
            .is_err()
    );
    assert!(
        guard
            .authorize(&req("https://public.test/", Purpose::Webhook, "acme"))
            .await
            .is_err(),
        "a tenant with no entry reaches nothing by URL"
    );
    // A public host on an ordinary port is what the default lets an address-based caller do.
    let ok = guard
        .authorize_addr("public.test", 443, Purpose::Mail, "acme", None)
        .await
        .unwrap();
    assert_eq!(ok.socket_addrs().len(), 1);
    // A dangerous port is not.
    assert!(matches!(
        guard
            .authorize_addr("public.test", 25, Purpose::Mail, "acme", None)
            .await,
        Err(EgressError::PortDenied { .. })
    ));
}

#[tokio::test]
async fn an_allow_entry_opens_one_host_one_port_one_purpose_for_one_tenant() {
    let dir = Dir::new("allow");
    let guard = build(&dir, Some(ALLOW_API), 60).guard();
    let ok = guard
        .authorize(&req(
            "https://api.example.test/hook",
            Purpose::Webhook,
            "acme",
        ))
        .await
        .unwrap();
    assert_eq!(ok.host(), "api.example.test");
    assert_eq!(ok.socket_addrs()[0].to_string(), "93.184.216.34:443");
    for (text, purpose, tenant) in [
        ("https://other.example.test/", Purpose::Webhook, "acme"),
        ("https://api.example.test:8443/", Purpose::Webhook, "acme"),
        ("https://api.example.test/", Purpose::Mail, "acme"),
        ("http://api.example.test/", Purpose::Webhook, "acme"),
        ("https://api.example.test/", Purpose::Webhook, "globex"),
    ] {
        let denied = guard.authorize(&req(text, purpose, tenant)).await;
        assert!(
            matches!(denied, Err(EgressError::PolicyDenied)),
            "{text} {tenant}: {denied:?}"
        );
    }
}

// --- the denial file -------------------------------------------------------------------------

#[tokio::test]
async fn a_denial_is_one_line_with_no_query_credentials_or_address() {
    let dir = Dir::new("line");
    let policy = r#"
[tenants.acme]
[[tenants.acme.allow]]
scheme = "https"
host = "internal.test"
"#;
    let host = build(&dir, Some(policy), 60);
    let guard = host.guard();
    let denied = guard
        .authorize(&req(
            "https://user:hunter2@internal.test:8443/path?token=token-canary#frag-canary",
            Purpose::Webhook,
            "acme",
        ))
        .await;
    assert!(matches!(denied, Err(EgressError::AddressDenied { .. })));
    let log = dir.path("denials.log");
    let written = lines(&log);
    assert_eq!(written.len(), 1, "one denial, one line");
    let line: DenialLine = serde_json::from_str(&written[0]).unwrap();
    assert_eq!(line.tenant, "acme");
    assert_eq!(line.purpose, "webhook");
    assert_eq!(line.actor.as_deref(), Some("svc-actor"));
    assert_eq!(line.scheme, "https");
    assert_eq!(line.host, "internal.test");
    assert_eq!(line.port, 8443);
    assert_eq!(line.class, "private");
    assert_eq!(line.rule, "address");
    assert_eq!(line.at, datetime!(2026-11-14 12:00:00 UTC).unix_timestamp());
    let text = std::fs::read_to_string(&log).unwrap();
    for canary in [
        "hunter2",
        "user:",
        "token-canary",
        "frag-canary",
        "/path",
        "10.0.0.1",
    ] {
        assert!(!text.contains(canary), "`{canary}` leaked into: {text}");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&log).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }
}

#[tokio::test]
async fn the_budget_drops_denials_over_it_and_counts_every_one() {
    let dir = Dir::new("budget");
    let host = build(&dir, None, 3);
    let status = EgressStatus::new(host.clone());
    let guard = host.guard();
    for _ in 0..10 {
        let _ = guard
            .authorize_addr("127.0.0.1", 80, Purpose::Webhook, "acme", None)
            .await;
    }
    let _ = guard
        .authorize_addr("127.0.0.1", 80, Purpose::Mail, "acme", None)
        .await;
    assert_eq!(
        lines(&dir.path("denials.log")).len(),
        4,
        "3 webhook and 1 mail"
    );
    assert_eq!(status.denied_total(), 11);
    let by = status.denied_by_purpose();
    assert_eq!(by["webhook"], 10);
    assert_eq!(by["mail"], 1);
    assert_eq!(by["llm"], 0);
    assert_eq!(status.suppressed(), 7);
    assert_eq!(status.write_failures(), 0);
}

#[tokio::test]
async fn a_failing_denial_file_is_counted_and_never_turns_a_denial_into_an_allow() {
    let dir = Dir::new("failing");
    // The log's parent is a regular file, so it can never be opened.
    std::fs::write(dir.path("blocker"), b"x").unwrap();
    let settings = Settings {
        denial_log: Some(dir.path("blocker").join("sub").join("denials.log")),
        ..Settings::default()
    };
    let paths = Paths::resolve(&settings, None, "acme", 1).unwrap();
    let host = EgressHost::build(&settings, paths, clock(), world()).unwrap();
    let status = EgressStatus::new(host.clone());
    assert!(!status.log_healthy());
    let guard = host.guard();
    let denied = guard
        .authorize_addr("127.0.0.1", 80, Purpose::Webhook, "acme", None)
        .await;
    assert!(matches!(denied, Err(EgressError::AddressDenied { .. })));
    assert!(status.write_failures() >= 1);
    assert_eq!(status.denied_total(), 1, "counted even though not written");
    // A permitted request is untouched by the broken file.
    assert!(
        guard
            .authorize_addr("public.test", 443, Purpose::Mail, "acme", None)
            .await
            .is_ok()
    );
    let health = host.check().await;
    assert!(
        health
            .as_ref()
            .err()
            .is_some_and(|m| m.contains(codes::DENIAL_LOG_UNWRITABLE)),
        "{health:?}"
    );
    assert_eq!(host.level(), Level::Optional);
}

// --- the policy file -------------------------------------------------------------------------

#[tokio::test]
async fn an_invalid_policy_file_refuses_to_start_and_lists_every_problem() {
    let dir = Dir::new("invalid");
    let bad = r#"
default = "permit"
[tenants.acme]
total_timeout_secs = 0
[[tenants.acme.allow]]
host = "a*b"
ports = [0]
purpose = "telnet"
"#;
    let (settings, paths) = settings(&dir, Some(bad), 60);
    let error = EgressHost::build(&settings, paths, clock(), world())
        .err()
        .unwrap()
        .to_string();
    assert!(error.starts_with(codes::POLICY_INVALID), "{error}");
    for expected in [
        "default:",
        "total_timeout_secs",
        ".host:",
        ".ports:",
        ".purpose:",
    ] {
        assert!(
            error.contains(expected),
            "`{expected}` missing from: {error}"
        );
    }
    // The registry path refuses the same way and registers nothing.
    let mut table = toml::map::Map::new();
    table.insert(
        "policy_file".into(),
        toml::Value::String(dir.path("policy.toml").display().to_string()),
    );
    table.insert(
        "denial_log".into(),
        toml::Value::String(dir.path("denials.log").display().to_string()),
    );
    let mut tables = BTreeMap::new();
    tables.insert("egress".to_owned(), toml::Value::Table(table));
    let mut ctx = SiteCtx::new("acme", Duration::from_secs(1), clock(), tables, true);
    ctx.as_module("egress", svc::ORDER);
    assert!(svc::register(&mut ctx).is_err());
    assert!(ctx.port::<Guard>().is_none());
}

#[tokio::test]
async fn a_registered_site_provides_the_guard_and_the_status_ports() {
    let dir = Dir::new("register");
    std::fs::write(dir.path("policy.toml"), ALLOW_API).unwrap();
    let mut table = toml::map::Map::new();
    table.insert(
        "policy_file".into(),
        toml::Value::String(dir.path("policy.toml").display().to_string()),
    );
    table.insert(
        "denial_log".into(),
        toml::Value::String(dir.path("denials.log").display().to_string()),
    );
    let mut tables = BTreeMap::new();
    tables.insert("egress".to_owned(), toml::Value::Table(table));
    let mut ctx = SiteCtx::new("acme", Duration::from_secs(1), clock(), tables, true);
    ctx.as_module("egress", svc::ORDER);
    svc::register(&mut ctx).unwrap();
    // A later module takes both ports; an earlier one would get the registry's error.
    ctx.as_module("webhooks", 1200);
    assert!(ctx.require_port::<Guard>().is_ok());
    let status = ctx.require_port::<EgressStatus>().unwrap();
    assert_eq!(status.tenants(), vec!["acme".to_owned()]);
    assert_eq!(status.digest().len(), 64);
    assert_eq!(status.generation(), 1);
    let assembled = ctx.finish(axum::Router::new());
    let services = assembled.assembly.expect("an assembling build");
    assert_eq!(services.services().len(), 1);
    assert_eq!(services.services()[0].name(), "egress");
    assert_eq!(services.services()[0].level(), Level::Optional);
}

#[tokio::test]
async fn a_reload_that_does_not_parse_keeps_the_old_policy_until_a_good_one_arrives() {
    let dir = Dir::new("reload");
    let host = build(&dir, Some(ALLOW_API), 60);
    let status = EgressStatus::new(host.clone());
    let first = status.digest();
    let allowed = |host: Arc<EgressHost>, name: &'static str| async move {
        host.guard()
            .authorize(&req(&format!("https://{name}/"), Purpose::Webhook, "acme"))
            .await
            .is_ok()
    };
    assert!(allowed(host.clone(), "api.example.test").await);

    std::fs::write(dir.path("policy.toml"), "this is = = not toml").unwrap();
    let problem = host.reload().unwrap_err();
    assert_eq!(problem.code, codes::POLICY_INVALID);
    assert_eq!(status.generation(), 1);
    assert_eq!(status.digest(), first);
    assert!(!status.reload_problems().is_empty());
    assert!(
        allowed(host.clone(), "api.example.test").await,
        "the old policy stays"
    );
    let health = host.check().await;
    assert!(
        health
            .as_ref()
            .err()
            .is_some_and(|m| m.contains(codes::RELOAD_FAILED)),
        "{health:?}"
    );

    let moved = ALLOW_API.replace("api.example.test", "other.example.test");
    std::fs::write(dir.path("policy.toml"), moved).unwrap();
    assert_eq!(host.reload().unwrap(), Reloaded::Applied);
    assert_eq!(status.generation(), 2);
    assert_ne!(status.digest(), first);
    assert!(status.reload_problems().is_empty());
    assert!(allowed(host.clone(), "other.example.test").await);
    assert!(!allowed(host.clone(), "api.example.test").await);
    assert!(host.check().await.is_ok());
    // The same file again changes nothing.
    assert_eq!(host.reload().unwrap(), Reloaded::Unchanged);
    assert_eq!(status.generation(), 2);
}

#[tokio::test]
async fn the_running_service_applies_a_reload_marker() {
    let dir = Dir::new("marker");
    let host = build(&dir, Some(ALLOW_API), 60);
    let status = EgressStatus::new(host.clone());
    let (stop, watch) = tokio::sync::watch::channel(false);
    let runner = {
        let host = host.clone();
        tokio::spawn(async move { host.run(watch).await })
    };
    let moved = ALLOW_API.replace("api.example.test", "other.example.test");
    std::fs::write(dir.path("policy.toml"), moved).unwrap();
    svc::request_reload(&host.paths().reload_marker).unwrap();
    let mut applied = false;
    for _ in 0..60 {
        if status.generation() == 2 {
            applied = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    stop.send(true).unwrap();
    runner.await.unwrap().unwrap();
    assert!(applied, "the marker was not picked up within six seconds");
    assert!(
        !host.paths().reload_marker.exists(),
        "the marker is consumed"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn the_policy_files_mode_and_existence_are_checked() {
    use std::os::unix::fs::PermissionsExt;
    let dir = Dir::new("mode");
    let (settings, paths) = settings(&dir, Some(ALLOW_API), 60);
    let file = dir.path("policy.toml");

    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o666)).unwrap();
    let error = EgressHost::build(&settings, paths.clone(), clock(), world())
        .err()
        .unwrap()
        .to_string();
    assert!(error.starts_with(codes::POLICY_UNSAFE), "{error}");

    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o640)).unwrap();
    assert!(EgressHost::build(&settings, paths.clone(), clock(), world()).is_ok());

    std::fs::remove_file(&file).unwrap();
    let error = EgressHost::build(&settings, paths, clock(), world())
        .err()
        .unwrap()
        .to_string();
    assert!(error.starts_with(codes::POLICY_UNREADABLE), "{error}");
}

// --- the command -----------------------------------------------------------------------------

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
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }
}

/// A host root with one site `acme` and `[services.egress]` pointing at the directory's files.
fn root(dir: &Dir, policy: Option<&str>) -> PathBuf {
    let root = dir.path("root");
    std::fs::create_dir_all(root.join("apps/books/doctypes")).unwrap();
    std::fs::create_dir_all(root.join("sites/acme")).unwrap();
    std::fs::write(
        root.join("sites/acme/site.toml"),
        "site = 'acme'\ndatabase = 'acme'\n",
    )
    .unwrap();
    let mut common = format!(
        "apps = [{:?}]\n[server]\nbind = ['127.0.0.1:0']\n[postgres]\nadmin_url = 'postgres://u:p@127.0.0.1:15432/postgres'\n[services.egress]\ndenial_log = {:?}\n",
        root.join("apps/books").display().to_string(),
        dir.path("denials.log").display().to_string(),
    );
    if let Some(text) = policy {
        std::fs::write(dir.path("policy.toml"), text).unwrap();
        common.push_str(&format!(
            "policy_file = {:?}\n",
            dir.path("policy.toml").display().to_string()
        ));
    }
    std::fs::write(root.join("common.toml"), common).unwrap();
    root
}

async fn egress_cmd(root: &Path, args: &[&str]) -> (Exit, String) {
    let (out, err) = (Sink::default(), Sink::default());
    let mut ctx = CommandCtx::new(
        root.to_path_buf(),
        clock(),
        Box::new(out.clone()),
        Box::new(err.clone()),
    );
    let registration = commands::COMMANDS
        .iter()
        .find(|r| r.name == "egress")
        .expect("the egress command registers itself");
    let mut argv = vec!["egress"];
    argv.extend_from_slice(args);
    let matches = (registration.command)()
        .try_get_matches_from(argv)
        .expect("the arguments parse");
    let exit = match (registration.run)(&matches, &mut ctx).await {
        Ok(exit) => exit,
        Err(e) => {
            let _ = writeln!(ctx.err(), "{e}");
            Exit::Failed
        }
    };
    (exit, format!("{}{}", out.text(), err.text()))
}

const CHECK_POLICY: &str = r#"
[tenants.acme]
[[tenants.acme.allow]]
scheme = "https"
host = "8.8.8.8"
ports = [443]
purpose = "webhook"

[[tenants.acme.allow]]
scheme = "http"
host = "127.0.0.1"
ports = [80]
purpose = "webhook"
"#;

#[tokio::test]
async fn check_prints_and_exits_as_documented() {
    let dir = Dir::new("check");
    let root = root(&dir, Some(CHECK_POLICY));

    let (exit, text) = egress_cmd(
        &root,
        &[
            "check",
            "https://8.8.8.8/x?secret=canary",
            "--purpose",
            "webhook",
        ],
    )
    .await;
    assert_eq!(exit, Exit::Ok, "{text}");
    assert!(text.contains("allowed: https://8.8.8.8:443"), "{text}");
    assert!(text.contains("8.8.8.8:443"), "{text}");
    assert!(!text.contains("canary"), "{text}");

    // Not in the allow-list: refused by policy.
    let (exit, text) = egress_cmd(
        &root,
        &["check", "https://8.8.4.4/", "--purpose", "webhook"],
    )
    .await;
    assert_eq!(exit, Exit::Refused, "{text}");
    assert!(
        text.contains("denied:") && text.contains("rule: policy"),
        "{text}"
    );
    assert!(text.contains("code: egress.denied_by_policy"), "{text}");

    // Allowed by policy but loopback: refused by the address class, the address not printed.
    let (exit, text) = egress_cmd(
        &root,
        &["check", "http://127.0.0.1/", "--purpose", "webhook"],
    )
    .await;
    assert_eq!(exit, Exit::Refused, "{text}");
    assert!(text.contains("rule: address"), "{text}");
    assert!(text.contains("address class: loopback"), "{text}");

    // Another tenant has no entry at all.
    let (exit, _) = egress_cmd(
        &root,
        &[
            "check",
            "https://8.8.8.8/",
            "--purpose",
            "webhook",
            "--tenant",
            "globex",
        ],
    )
    .await;
    assert_eq!(exit, Exit::Refused);

    // Usage.
    let (exit, text) =
        egress_cmd(&root, &["check", "ftp://8.8.8.8/", "--purpose", "webhook"]).await;
    assert_eq!(exit, Exit::Usage, "{text}");
    assert!(text.contains(codes::URL_INVALID), "{text}");
    let (exit, text) =
        egress_cmd(&root, &["check", "https://8.8.8.8/", "--purpose", "telnet"]).await;
    assert_eq!(exit, Exit::Usage, "{text}");
    assert!(text.contains(codes::PURPOSE_UNKNOWN), "{text}");

    // `check` records nothing: an operator's probe is not a tenant's denial.
    assert!(lines(&dir.path("denials.log")).is_empty());
    assert_eq!(Exit::Ok.code(), 0);
    assert_eq!(Exit::Usage.code(), 1);
    assert_eq!(Exit::Refused.code(), 2);
    assert_eq!(Exit::Failed.code(), 3);
}

#[tokio::test]
async fn check_with_an_invalid_policy_file_fails_and_lists_the_problems() {
    let dir = Dir::new("check_bad");
    let root = root(&dir, Some("default = \"permit\"\n"));
    let (exit, text) = egress_cmd(
        &root,
        &["check", "https://8.8.8.8/", "--purpose", "webhook"],
    )
    .await;
    assert_eq!(exit, Exit::Failed, "{text}");
    assert!(
        text.contains(codes::POLICY_INVALID) && text.contains("default:"),
        "{text}"
    );
}

#[tokio::test]
async fn denials_reads_newest_first_and_since_filters() {
    let dir = Dir::new("denials");
    let root = root(&dir, None);
    let now = clock().now_utc().unix_timestamp();
    let make = |at: i64, purpose: &str, host: &str| DenialLine {
        at,
        tenant: "acme".into(),
        purpose: purpose.into(),
        actor: None,
        scheme: "https".into(),
        host: host.into(),
        port: 443,
        class: "private".into(),
        rule: "address".into(),
    };
    let body = [
        make(now - 7200, "webhook", "old.test"),
        make(now - 600, "mail", "mid.test"),
        make(now - 60, "webhook", "new.test"),
    ]
    .iter()
    .map(|l| serde_json::to_string(l).unwrap())
    .collect::<Vec<_>>()
    .join("\n");
    std::fs::write(
        dir.path("denials.log"),
        format!("{body}\nnot json at all\n"),
    )
    .unwrap();

    let hosts = |text: &str| -> Vec<String> {
        text.lines()
            .filter(|l| l.starts_with('{'))
            .map(|l| serde_json::from_str::<DenialLine>(l).unwrap().host)
            .collect()
    };
    let (exit, text) = egress_cmd(&root, &["denials", "--json"]).await;
    assert_eq!(exit, Exit::Ok, "{text}");
    assert_eq!(hosts(&text), ["new.test", "mid.test", "old.test"]);

    let (_, text) = egress_cmd(&root, &["denials", "--json", "--since", "1h"]).await;
    assert_eq!(hosts(&text), ["new.test", "mid.test"]);

    let (_, text) = egress_cmd(&root, &["denials", "--json", "--since", "30s"]).await;
    assert!(hosts(&text).is_empty());

    let (_, text) = egress_cmd(&root, &["denials", "--json", "--purpose", "mail"]).await;
    assert_eq!(hosts(&text), ["mid.test"]);

    let (_, text) = egress_cmd(&root, &["denials", "--json", "--limit", "1"]).await;
    assert_eq!(hosts(&text), ["new.test"]);

    let (exit, text) = egress_cmd(&root, &["denials", "--since", "30s"]).await;
    assert_eq!(exit, Exit::Ok);
    assert!(text.contains("no denials recorded"), "{text}");

    let (exit, text) = egress_cmd(&root, &["denials"]).await;
    assert_eq!(exit, Exit::Ok);
    assert!(
        text.contains("host") || text.contains("https://new.test:443"),
        "{text}"
    );
    assert!(text.contains("rule=address class=private"), "{text}");

    let (exit, text) = egress_cmd(&root, &["denials", "--since", "soon"]).await;
    assert_eq!(exit, Exit::Usage, "{text}");
    assert!(text.contains(codes::QUERY_INVALID), "{text}");
}

#[tokio::test]
async fn a_hostile_host_name_cannot_write_control_sequences_to_the_terminal() {
    let dir = Dir::new("escape");
    let root = root(&dir, None);
    let line = DenialLine {
        at: clock().now_utc().unix_timestamp(),
        tenant: "acme".into(),
        purpose: "webhook".into(),
        actor: Some("a\u{1b}[31mb".into()),
        scheme: "https".into(),
        host: "evil\u{1b}]0;pwned\u{7}.test".into(),
        port: 443,
        class: "private".into(),
        rule: "address".into(),
    };
    std::fs::write(
        dir.path("denials.log"),
        format!("{}\n", serde_json::to_string(&line).unwrap()),
    )
    .unwrap();
    let (exit, text) = egress_cmd(&root, &["denials"]).await;
    assert_eq!(exit, Exit::Ok);
    assert!(
        !text.contains('\u{1b}') && !text.contains('\u{7}'),
        "{text:?}"
    );
}

#[tokio::test]
async fn reload_validates_first_and_asks_the_server_only_for_a_good_file() {
    let dir = Dir::new("reload_cmd");
    let root = root(&dir, Some(ALLOW_API));
    let marker = dir.path("denials.log.reload");

    let (exit, text) = egress_cmd(&root, &["reload"]).await;
    assert_eq!(exit, Exit::Ok, "{text}");
    assert!(text.contains("1 tenant(s)"), "{text}");
    assert!(marker.exists());
    assert!(svc::take_reload(&marker));
    assert!(!svc::take_reload(&marker), "a marker is consumed once");

    std::fs::write(dir.path("policy.toml"), "default = \"permit\"\n").unwrap();
    let (exit, text) = egress_cmd(&root, &["reload"]).await;
    assert_eq!(exit, Exit::Refused, "{text}");
    assert!(text.contains(codes::POLICY_INVALID), "{text}");
    assert!(!marker.exists(), "nothing was asked of the server");
}

#[tokio::test]
async fn with_no_policy_file_reload_has_nothing_to_do() {
    let dir = Dir::new("reload_none");
    let root = root(&dir, None);
    let (exit, text) = egress_cmd(&root, &["reload"]).await;
    assert_eq!(exit, Exit::Ok, "{text}");
    assert!(text.contains("nothing to reload"), "{text}");
}
