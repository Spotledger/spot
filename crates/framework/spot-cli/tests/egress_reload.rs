//! WP-SEC-09.T10: a policy reload swaps the guard behind one shared handle. A holder cloned
//! before the reload sees the new policy, a failed reload changes nothing, every swap is
//! audited, and a decision made during a swap sees the old or the new policy, never a mix.
//! No network and no database: DNS is scripted and every file lives in a temporary directory.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashMap;
use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use spot_cli::services::egress::{EgressHost, EgressStatus, Paths, Reloaded, Settings, codes};
use spot_core::ManualClock;
use spot_egress::{EgressError, Purpose, Request, Resolver};
use time::macros::datetime;

static COUNTER: AtomicU32 = AtomicU32::new(0);

struct Dir(PathBuf);

impl Dir {
    fn new() -> Self {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir =
            std::env::temp_dir().join(format!("spot_egress_reload_{}_{n}", std::process::id()));
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

fn world() -> Arc<StaticResolver> {
    let ip: IpAddr = "93.184.216.34".parse().unwrap();
    Arc::new(StaticResolver(
        ["api.example.test", "other.example.test"]
            .iter()
            .map(|h| ((*h).to_owned(), vec![ip]))
            .collect(),
    ))
}

/// An entry without a scheme covers `https` and the address-based (non-HTTP) decisions the test
/// also makes; an entry for `https` alone would deny `authorize_addr` by design.
fn allow_only(host: &str) -> String {
    format!(
        r#"
[tenants.acme]
[[tenants.acme.allow]]
host = "{host}"
ports = [443]
purpose = "webhook"
"#
    )
}

struct Fixture {
    dir: Dir,
    host: Arc<EgressHost>,
}

fn fixture(policy: &str) -> Fixture {
    let dir = Dir::new();
    let policy_path = dir.path("policy.toml");
    std::fs::write(&policy_path, policy).unwrap();
    let settings = Settings {
        policy_file: Some(policy_path),
        denial_log: Some(dir.path("denials.log")),
        denial_log_max_bytes: None,
        denials_per_minute: Some(60),
    };
    let paths = Paths::resolve(&settings, None, "acme", 1).unwrap();
    let clock = Arc::new(ManualClock::new(datetime!(2026-11-14 12:00:00 UTC)));
    let host = EgressHost::build(&settings, paths, clock, world()).unwrap();
    Fixture { dir, host }
}

impl Fixture {
    fn write_policy(&self, text: &str) {
        std::fs::write(self.dir.path("policy.toml"), text).unwrap();
    }
}

fn req(host: &str) -> Request {
    Request {
        url: format!("https://{host}/x").parse().unwrap(),
        purpose: Purpose::Webhook,
        tenant: "acme".to_owned(),
        actor: None,
    }
}

#[tokio::test]
async fn a_holder_cloned_before_the_reload_sees_the_new_policy() {
    let fx = fixture(&allow_only("api.example.test"));
    let holder = fx.host.shared();
    assert!(holder.authorize(&req("api.example.test")).await.is_ok());
    assert!(matches!(
        holder.authorize(&req("other.example.test")).await,
        Err(EgressError::PolicyDenied)
    ));

    fx.write_policy(&allow_only("other.example.test"));
    assert_eq!(fx.host.reload().unwrap(), Reloaded::Applied);

    // The same handle: the removed allow is denied, the new allow works.
    assert!(matches!(
        holder.authorize(&req("api.example.test")).await,
        Err(EgressError::PolicyDenied)
    ));
    assert!(holder.authorize(&req("other.example.test")).await.is_ok());
    // The identity never changed: the status hands out the same handle.
    let status = EgressStatus::new(fx.host.clone());
    assert!(Arc::ptr_eq(&holder, &status.shared()));
    assert!(Arc::ptr_eq(&holder, &fx.host.shared()));
}

#[tokio::test]
async fn address_and_uri_decisions_follow_the_reload_too() {
    let fx = fixture(&allow_only("api.example.test"));
    let holder = fx.host.shared();
    let uri: http::Uri = "https://api.example.test/x".parse().unwrap();
    assert!(
        holder
            .authorize_uri(&uri, Purpose::Webhook, "acme", None)
            .await
            .is_ok()
    );
    assert!(
        holder
            .authorize_addr("api.example.test", 443, Purpose::Webhook, "acme", None)
            .await
            .is_ok()
    );
    fx.write_policy(&allow_only("other.example.test"));
    fx.host.reload().unwrap();
    assert!(
        holder
            .authorize_uri(&uri, Purpose::Webhook, "acme", None)
            .await
            .is_err()
    );
    assert!(
        holder
            .authorize_addr("api.example.test", 443, Purpose::Webhook, "acme", None)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn a_bad_reload_changes_nothing_and_reports_the_catalogued_error() {
    let fx = fixture(&allow_only("api.example.test"));
    let holder = fx.host.shared();
    let before = holder.current();

    fx.write_policy("this is = not [valid toml");
    let problem = fx.host.reload().unwrap_err();
    assert!(problem.code.starts_with("egress_host."), "{}", problem.code);
    assert!(!problem.problems.is_empty());

    // The old guard is still the one in force, and nothing was audited.
    assert!(Arc::ptr_eq(&before, &holder.current()));
    assert!(holder.authorize(&req("api.example.test")).await.is_ok());
    assert!(holder.swaps().is_empty());
    let status = EgressStatus::new(fx.host.clone());
    assert_eq!(status.generation(), 1);
    assert!(!status.reload_problems().is_empty());

    // A good file afterwards swaps and clears the failure.
    fx.write_policy(&allow_only("other.example.test"));
    assert_eq!(fx.host.reload().unwrap(), Reloaded::Applied);
    assert!(status.reload_problems().is_empty());
    assert_eq!(status.generation(), 2);
    let _ = codes::RELOAD_FAILED;
}

#[tokio::test]
async fn each_swap_writes_one_audit_record_of_names_and_counts() {
    let fx = fixture(&allow_only("api.example.test"));
    let holder = fx.host.shared();
    assert_eq!(fx.host.reload().unwrap(), Reloaded::Unchanged);
    assert!(holder.swaps().is_empty());

    fx.write_policy(&allow_only("other.example.test"));
    fx.host.reload().unwrap();
    fx.write_policy(&allow_only("api.example.test"));
    fx.host.reload().unwrap();

    let swaps = holder.swaps();
    assert_eq!(swaps.len(), 2);
    assert_eq!(swaps[0].generation, 2);
    assert_eq!(swaps[1].generation, 3);
    for swap in &swaps {
        assert_eq!(swap.tenants, 1);
        assert_eq!(swap.previous_tenants, 1);
        assert_eq!(swap.digest.len(), 12);
        let text = format!("{swap:?}");
        assert!(!text.contains("example.test"), "{text}");
    }
    assert_ne!(swaps[0].digest, swaps[1].digest);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_decision_during_swaps_sees_the_old_or_the_new_policy_never_a_mix() {
    let fx = fixture(&allow_only("api.example.test"));
    let holder = fx.host.shared();
    let stop = Arc::new(AtomicBool::new(false));

    let mut checkers = Vec::new();
    for _ in 0..3 {
        let holder = holder.clone();
        let stop = stop.clone();
        checkers.push(tokio::spawn(async move {
            let mut seen = 0u32;
            while !stop.load(Ordering::Relaxed) {
                // One snapshot answers both questions: policy A allows only api, B only other.
                let guard = holder.current();
                let api = guard.authorize(&req("api.example.test")).await.is_ok();
                let other = guard.authorize(&req("other.example.test")).await.is_ok();
                assert!(api != other, "mixed policy: api={api} other={other}");
                seen += 1;
                tokio::task::yield_now().await;
            }
            seen
        }));
    }

    let host = fx.host.clone();
    let a = allow_only("api.example.test");
    let b = allow_only("other.example.test");
    let dir_policy = fx.dir.path("policy.toml");
    let swapper = tokio::task::spawn_blocking(move || {
        for i in 0..40 {
            let text = if i % 2 == 0 { &b } else { &a };
            std::fs::write(&dir_policy, text).unwrap();
            assert_eq!(host.reload().unwrap(), Reloaded::Applied);
        }
    });
    swapper.await.unwrap();
    stop.store(true, Ordering::Relaxed);
    for checker in checkers {
        assert!(checker.await.unwrap() > 0);
    }
    assert_eq!(holder.swaps().len(), 32);
}
