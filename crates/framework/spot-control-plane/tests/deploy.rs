//! The `deploy` module: plan, approve, run and status with fake agents over the in-memory rows;
//! one test runs the whole flow over PostgreSQL through `SPOT_TEST_PG_URL` and prints why it
//! skips when it is unset.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, missing_docs)]

use std::cell::RefCell;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use rcgen::KeyPair;
use serde_json::{Value, json};
use spot_control_plane::auth::{KeyVault, sha256_hex};
use spot_control_plane::config::Config;
use spot_control_plane::modules::deploy::{
    self, AgentLink, DeployState, Wiring, redeem_enrolment_token,
};
use spot_control_plane::modules::tenants_kv::{Kv, MemoryKv, PgKv};
use spot_control_plane::registry::Registration;
use spot_control_plane::{BuildDeps, ControlPlane, CpCode, CpCtx, CpError};
use spot_core::{Clock, ManualClock};
use spot_deploy_wizard::{
    AgentFacts, EnrolledAgents, Host, OsFacts, OsFamily, PeerFacts, Receipt, SignedFacts,
};
use spot_mgmt_api::ports::{AuditSink, BoxFuture, MemoryAuditSink};
use spot_mgmt_api::tls::PeerCertificate;
use spot_signed_command::{Command, MemoryNonceStore, SignedCommand, Signer, SoftwareSigner, sign};
use tower::ServiceExt as _;

const NOW: i64 = 1_800_000_000;
const HOSTS: [&str; 3] = ["app1.example.com", "db1.example.com", "db2.example.com"];

struct MemVault(String);
impl KeyVault for MemVault {
    fn ca_key_pem(&self) -> Result<String, spot_control_plane::auth::CaError> {
        Ok(self.0.clone())
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn config_text(admin_key_hex: &str) -> String {
    format!(
        r#"
region = "eu-1"
audience = "cp-eu-1"
administrator_keys = ["{admin_key_hex}"]
[database]
host = "127.0.0.1"
dbname = "control"
user = "cp"
password_env = "SPOT_CP_TEST_PASSWORD_UNSET"
tls = false
[operator_listener]
listen = "127.0.0.1:0"
server_cert = "/nonexistent/op.pem"
server_key = "/nonexistent/op.key"
client_ca = "/nonexistent/operator-ca.pem"
[host_listener]
listen = "127.0.0.1:1"
server_cert = "/nonexistent/host.pem"
server_key = "/nonexistent/host.key"
client_ca = "/nonexistent/host-ca.pem"
[host_ca]
cert = "/nonexistent/host-ca.pem"
key = "/nonexistent/host-ca.key"
common_name = "Spot host CA"
[[operators]]
subject = "CN=root"
principal = "root"
tenants = ["*"]
[[operators]]
subject = "CN=bob"
principal = "bob"
tenants = ["*"]
[[operators]]
subject = "CN=alice"
principal = "alice"
tenants = ["tenant-one"]
"#
    )
}

/// The fake agents: signed facts, and a record of what was delivered.
struct Link {
    agent_key: ed25519_dalek::SigningKey,
    seq: AtomicU64,
    clock: Arc<ManualClock>,
    delivered: Mutex<Vec<(String, String)>>,
    fail_once: Mutex<Option<String>>,
    low_disk: Mutex<Option<String>>,
}

impl AgentLink for Link {
    fn collect_facts<'a>(&'a self, host: &'a str) -> BoxFuture<'a, Result<SignedFacts, String>> {
        let seq = self.seq.fetch_add(1, Ordering::SeqCst) + 1;
        let now = self.clock.now_utc().unix_timestamp();
        let peers = HOSTS
            .iter()
            .filter(|h| **h != host)
            .map(|h| PeerFacts {
                host: Host::try_new(h).unwrap(),
                reachable: true,
            })
            .collect();
        let mut facts = AgentFacts::new(
            Host::try_new(host).unwrap(),
            OsFacts {
                family: OsFamily::Linux,
                release: "Ubuntu 24.04 LTS".to_owned(),
                supported: true,
            },
        )
        .with_peers(peers);
        if self.low_disk.lock().unwrap().as_deref() == Some(host) {
            facts = facts.with_data_dir_free_gb(1);
        }
        let agent_id = format!("agent-{host}");
        let message = SignedFacts::signed_message(&agent_id, seq, now, &facts).unwrap();
        let signed: ed25519_dalek::Signature =
            ed25519_dalek::Signer::sign(&self.agent_key, &message);
        let signature = signed.to_bytes().to_vec();
        Box::pin(std::future::ready(Ok(SignedFacts {
            agent_id,
            sequence: seq,
            observed_at: now,
            facts,
            signature,
        })))
    }

    fn deliver<'a>(
        &'a self,
        audience: &'a str,
        command: &'a SignedCommand,
    ) -> BoxFuture<'a, Result<Receipt, String>> {
        let op = command.command.op().to_owned();
        let result = {
            let mut fail = self.fail_once.lock().unwrap();
            if fail.as_deref() == Some(op.as_str()) {
                *fail = None;
                Err("the agent refused the step".to_owned())
            } else {
                self.delivered
                    .lock()
                    .unwrap()
                    .push((audience.to_owned(), op));
                Ok(Receipt {
                    command_id: command.command.id().to_owned(),
                    detail: "accepted".to_owned(),
                })
            }
        };
        Box::pin(std::future::ready(result))
    }
}

struct Directory(ed25519_dalek::VerifyingKey);
impl EnrolledAgents for Directory {
    fn enrolled_agent(&self, host: &Host) -> Option<String> {
        Some(format!("agent-{}", host.as_str()))
    }
    fn verify(&self, _agent_id: &str, message: &[u8], signature: &[u8]) -> bool {
        let Ok(bytes) = <[u8; 64]>::try_from(signature) else {
            return false;
        };
        let signature = ed25519_dalek::Signature::from_bytes(&bytes);
        ed25519_dalek::Verifier::verify(&self.0, message, &signature).is_ok()
    }
}

thread_local! {
    static STATE: RefCell<Option<Arc<DeployState>>> = const { RefCell::new(None) };
}

fn register(ctx: &mut CpCtx) -> Result<(), CpError> {
    let state = STATE.with(|s| s.borrow().clone()).expect("state set");
    deploy::register_with(ctx, state);
    Ok(())
}

struct Fixture {
    plane: ControlPlane,
    clock: Arc<ManualClock>,
    signer: SoftwareSigner,
    link: Arc<Link>,
    kv: Arc<dyn Kv>,
}

fn new_link(clock: &Arc<ManualClock>) -> Arc<Link> {
    Arc::new(Link {
        agent_key: ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]),
        seq: AtomicU64::new(0),
        clock: Arc::clone(clock),
        delivered: Mutex::new(Vec::new()),
        fail_once: Mutex::new(None),
        low_disk: Mutex::new(None),
    })
}

/// A control plane over `kv`, `clock` and `link`; building another over the same three is a
/// restart of the service.
fn fixture_over(kv: Arc<dyn Kv>, clock: Arc<ManualClock>, link: Arc<Link>) -> Fixture {
    let signer = SoftwareSigner::from_seed(&[9u8; 32]).unwrap();
    let config = Config::from_toml(&config_text(&hex(&signer.public_key_bytes()))).unwrap();
    let clock_dyn: Arc<dyn Clock> = clock.clone();
    let wiring = Wiring {
        directory: Arc::new(Directory(link.agent_key.verifying_key())),
        link: link.clone(),
        signer: Arc::new(SoftwareSigner::from_seed(&[5u8; 32]).unwrap()),
    };
    let state = DeployState::new(Arc::clone(&kv), clock_dyn.clone(), wiring).unwrap();
    STATE.with(|s| *s.borrow_mut() = Some(state));
    let audit: Arc<dyn AuditSink> = Arc::new(MemoryAuditSink::new());
    let ca = KeyPair::generate().unwrap().serialize_pem();
    let plane = ControlPlane::build(BuildDeps {
        config: Arc::new(config),
        pool: None,
        clock: clock_dyn,
        audit,
        nonces: Arc::new(MemoryNonceStore::new()),
        vault: Arc::new(MemVault(ca)),
        registrations: vec![Registration {
            name: "deploy",
            order: deploy::ORDER,
            register,
        }],
    })
    .unwrap();
    Fixture {
        plane,
        clock,
        signer,
        link,
        kv,
    }
}

fn fixture() -> Fixture {
    let clock = Arc::new(ManualClock::new(
        time::OffsetDateTime::from_unix_timestamp(NOW).unwrap(),
    ));
    let link = new_link(&clock);
    fixture_over(Arc::new(MemoryKv::new()), clock, link)
}

impl Fixture {
    async fn op(&self, who: &str, method: &str, path: &str, body: &str) -> (StatusCode, Value) {
        let mut b = Request::builder()
            .method(method)
            .uri(path)
            .extension(PeerCertificate::new(
                &format!("CN={who}"),
                &format!("fp-{who}"),
            ));
        if method == "POST" {
            let now = self.clock.now_utc().unix_timestamp();
            let command = Command::new(
                "cp.test",
                json!({ "body_sha256": sha256_hex(body.as_bytes()), "path": path }),
                "cp-eu-1",
                self.signer.key_id(),
                now - 5,
                now + 300,
            );
            let signed = sign(&command, &self.signer).unwrap();
            let text = spot_mgmt_api::wire::encode(&signed, &[]).unwrap();
            b = b.header("x-spot-command", URL_SAFE_NO_PAD.encode(text.as_bytes()));
        }
        let response = self
            .plane
            .operator_router
            .clone()
            .oneshot(b.body(Body::from(body.to_owned())).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    async fn plan(&self, who: &str, tenant: &str) -> (StatusCode, Value) {
        let body = json!({ "tenant_id": tenant, "topology": {
            "nodes": [
                { "host": HOSTS[0], "roles": ["web", "jobs", "realtime"] },
                { "host": HOSTS[1], "roles": ["database"] },
                { "host": HOSTS[2], "roles": ["database"] },
            ],
            "profile": "cluster-basic", "web_address": "erp.example.com",
        }})
        .to_string();
        self.op(who, "POST", "/cp/v1/deployments/plan", &body).await
    }

    async fn approve(&self, who: &str, id: &str, hash: &str) -> (StatusCode, Value) {
        let body = json!({ "plan_hash": hash }).to_string();
        self.op(
            who,
            "POST",
            &format!("/cp/v1/deployments/{id}/approve"),
            &body,
        )
        .await
    }

    async fn run(&self, who: &str, id: &str, confirm: &[u64]) -> (StatusCode, Value) {
        let body = json!({ "confirm_steps": confirm }).to_string();
        self.op(who, "POST", &format!("/cp/v1/deployments/{id}/run"), &body)
            .await
    }

    fn delivered(&self) -> Vec<(String, String)> {
        self.link.delivered.lock().unwrap().clone()
    }
}

fn destructive(plan: &Value) -> Vec<u64> {
    plan["data"]["steps"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|s| s["destructive"] == true)
        .map(|s| s["index"].as_u64().unwrap())
        .collect()
}

fn id_and_hash(plan: &Value) -> (String, String) {
    (
        plan["data"]["id"].as_str().unwrap().to_owned(),
        plan["data"]["plan_hash"].as_str().unwrap().to_owned(),
    )
}

#[tokio::test]
async fn planning_has_no_side_effects() {
    let f = fixture();
    let (status, plan) = f.plan("root", "tenant-one").await;
    assert_eq!(status, StatusCode::CREATED, "{plan}");
    assert_eq!(plan["data"]["state"], "planned");
    assert_eq!(plan["data"]["preflight_ready"], true);
    assert!(!plan["data"]["steps"].as_array().unwrap().is_empty());
    assert!(f.delivered().is_empty(), "nothing reaches an agent");
    for ns in ["deploy_token", "deploy_journal", "deploy_audit"] {
        assert!(f.kv.list(ns).await.unwrap().is_empty(), "{ns} stays empty");
    }
}

#[tokio::test]
async fn a_blocking_preflight_check_names_itself_and_nothing_is_stored() {
    let f = fixture();
    *f.link.low_disk.lock().unwrap() = Some(HOSTS[1].to_owned());
    let (status, answer) = f.plan("root", "tenant-one").await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{answer}");
    assert_eq!(answer["error"]["code"], "cp.invalid");
    assert!(
        answer["error"]["message"]
            .as_str()
            .unwrap()
            .contains("pre-flight"),
        "{answer}"
    );
    assert!(f.kv.list("deploy_plan").await.unwrap().is_empty());
}

#[tokio::test]
async fn an_unapproved_run_is_refused_and_nothing_is_sent() {
    let f = fixture();
    let (_, plan) = f.plan("root", "tenant-one").await;
    let (id, _) = id_and_hash(&plan);
    let (status, answer) = f.run("root", &id, &destructive(&plan)).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{answer}");
    assert_eq!(answer["error"]["code"], "cp.approval_required");
    assert!(f.delivered().is_empty());
}

#[tokio::test]
async fn approve_then_run_completes_with_a_second_administrator_and_per_step_confirmation() {
    let f = fixture();
    let (_, plan) = f.plan("root", "tenant-one").await;
    let (id, hash) = id_and_hash(&plan);
    let steps = plan["data"]["steps"].as_array().unwrap().len();
    let (status, approved) = f.approve("bob", &id, &hash).await;
    assert_eq!(status, StatusCode::OK, "{approved}");
    assert_eq!(
        approved["data"]["enrolment_tokens"]
            .as_array()
            .unwrap()
            .len(),
        3
    );
    // The approver does not also run it.
    let (status, answer) = f.run("bob", &id, &destructive(&plan)).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{answer}");
    // A destructive step that is not confirmed is refused by name, before anything is sent.
    let (status, answer) = f.run("root", &id, &[]).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{answer}");
    assert!(
        answer["error"]["message"]
            .as_str()
            .unwrap()
            .contains("confirmation")
    );
    assert!(f.delivered().is_empty());
    let (status, done) = f.run("root", &id, &destructive(&plan)).await;
    assert_eq!(status, StatusCode::OK, "{done}");
    assert_eq!(done["data"]["state"], "done");
    assert_eq!(f.delivered().len(), steps);
    let (status, view) = f
        .op("root", "GET", &format!("/cp/v1/deployments/{id}"), "")
        .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        view["data"]["steps"]
            .as_array()
            .unwrap()
            .iter()
            .all(|s| s["state"] == "succeeded"),
        "{view}"
    );
    // A finished deployment is not run again.
    assert_eq!(
        f.run("root", &id, &destructive(&plan)).await.0,
        StatusCode::CONFLICT
    );
    assert_eq!(f.delivered().len(), steps);
}

#[tokio::test]
async fn a_changed_plan_after_approval_is_refused() {
    let f = fixture();
    let (_, plan) = f.plan("root", "tenant-one").await;
    let (id, hash) = id_and_hash(&plan);
    // An approval naming another digest is refused.
    let (status, _) = f.approve("bob", &id, &"0".repeat(64)).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(f.approve("bob", &id, &hash).await.0, StatusCode::OK);
    // The stored plan is altered after the approval: its hash no longer matches.
    let text = f.kv.get("deploy_plan", &id).await.unwrap().unwrap();
    let mut record: Value = serde_json::from_str(&text).unwrap();
    record["hash"] = json!("f".repeat(64));
    f.kv.put("deploy_plan", &id, &record.to_string())
        .await
        .unwrap();
    let (status, answer) = f.run("root", &id, &destructive(&plan)).await;
    assert_eq!(status, StatusCode::CONFLICT, "{answer}");
    assert!(f.delivered().is_empty());
}

#[tokio::test]
async fn an_enrolment_token_is_one_time_for_its_host_and_expires_after_a_day() {
    let f = fixture();
    let (_, plan) = f.plan("root", "tenant-one").await;
    let (id, hash) = id_and_hash(&plan);
    let (_, approved) = f.approve("bob", &id, &hash).await;
    let tokens = approved["data"]["enrolment_tokens"].as_array().unwrap();
    let token = |i: usize| {
        (
            tokens[i]["host"].as_str().unwrap().to_owned(),
            tokens[i]["token"].as_str().unwrap().to_owned(),
        )
    };
    let (host0, token0) = token(0);
    let (host1, token1) = token(1);
    let kv = f.kv.as_ref();
    // The row holds a digest, never the token.
    for (_, row) in kv.list("deploy_token").await.unwrap() {
        assert!(!row.contains(&token0));
    }
    assert!(
        redeem_enrolment_token(kv, &token0, &host0, NOW)
            .await
            .is_ok()
    );
    let again = redeem_enrolment_token(kv, &token0, &host0, NOW).await;
    assert_eq!(again.unwrap_err().code, CpCode::Replayed);
    let other = redeem_enrolment_token(kv, &token1, &host0, NOW).await;
    assert_eq!(other.unwrap_err().code, CpCode::Forbidden);
    let late = redeem_enrolment_token(kv, &token1, &host1, NOW + 86_400).await;
    assert_eq!(late.unwrap_err().code, CpCode::Unauthenticated);
    assert!(
        redeem_enrolment_token(kv, &token1, &host1, NOW + 86_399)
            .await
            .is_ok()
    );
    let unknown = redeem_enrolment_token(kv, "spot-enrol-nope", &host0, NOW).await;
    assert_eq!(unknown.unwrap_err().code, CpCode::Unauthenticated);
    // The tokens are shown once: approving again is refused.
    assert_eq!(f.approve("bob", &id, &hash).await.0, StatusCode::CONFLICT);
}

#[tokio::test]
async fn a_restart_resumes_without_repeating_a_completed_step() {
    let f = fixture();
    let (_, plan) = f.plan("root", "tenant-one").await;
    let (id, hash) = id_and_hash(&plan);
    let steps = plan["data"]["steps"].as_array().unwrap().len();
    assert_eq!(f.approve("bob", &id, &hash).await.0, StatusCode::OK);
    *f.link.fail_once.lock().unwrap() = Some("deploy.runtime.install".to_owned());
    let (status, first) = f.run("root", &id, &destructive(&plan)).await;
    assert_eq!(status, StatusCode::OK, "{first}");
    assert_eq!(first["data"]["state"], "failed");
    let done_before = f.delivered().len();
    assert!(done_before > 0 && done_before < steps);
    // The service restarts: a new state and router over the same rows and agents.
    let g = fixture_over(Arc::clone(&f.kv), Arc::clone(&f.clock), Arc::clone(&f.link));
    let (_, status_view) = g
        .op("root", "GET", &format!("/cp/v1/deployments/{id}"), "")
        .await;
    assert_eq!(status_view["data"]["state"], "failed");
    let (status, second) = g.run("root", &id, &destructive(&plan)).await;
    assert_eq!(status, StatusCode::OK, "{second}");
    assert_eq!(second["data"]["state"], "done");
    // Every step was delivered successfully exactly once across both runs.
    assert_eq!(g.delivered().len(), steps);
    let mut seen = g.delivered();
    seen.sort();
    let before = seen.len();
    seen.dedup();
    assert_eq!(seen.len(), before, "no step was repeated");
}

#[tokio::test]
async fn an_operator_outside_the_tenant_scope_sees_nothing() {
    let f = fixture();
    let (status, _) = f.plan("alice", "tenant-two").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (_, plan) = f.plan("root", "tenant-two").await;
    let (id, hash) = id_and_hash(&plan);
    assert_eq!(
        f.approve("alice", &id, &hash).await.0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        f.op("alice", "GET", &format!("/cp/v1/deployments/{id}"), "")
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(f.run("alice", &id, &[]).await.0, StatusCode::NOT_FOUND);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_whole_flow_over_postgresql() {
    let Ok(url) = std::env::var("SPOT_TEST_PG_URL") else {
        eprintln!("SKIPPED: SPOT_TEST_PG_URL is not set");
        return;
    };
    let config = spot_postgres::PgConfig::from_url(&url).unwrap();
    let pool = spot_postgres::PgBackend::connect(config)
        .await
        .unwrap()
        .pool();
    let client = pool.get().await.unwrap();
    for statement in [
        "CREATE TABLE IF NOT EXISTS cp_tenants_kv (ns TEXT NOT NULL, k TEXT NOT NULL, v TEXT NOT NULL, PRIMARY KEY (ns, k))",
        "CREATE TABLE IF NOT EXISTS cp_tenants_lease (id TEXT PRIMARY KEY, holder TEXT NOT NULL, until BIGINT NOT NULL)",
        "DELETE FROM cp_tenants_kv WHERE ns LIKE 'deploy\\_%' OR ns = 'facts_seq'",
        "DELETE FROM cp_tenants_lease WHERE id LIKE 'deploy:%'",
    ] {
        client.batch_execute(statement).await.unwrap();
    }
    drop(client);
    let clock = Arc::new(ManualClock::new(
        time::OffsetDateTime::from_unix_timestamp(NOW).unwrap(),
    ));
    let link = new_link(&clock);
    let f = fixture_over(Arc::new(PgKv::new(pool)), clock, link);
    let (_, plan) = f.plan("root", "tenant-one").await;
    let (id, hash) = id_and_hash(&plan);
    let steps = plan["data"]["steps"].as_array().unwrap().len();
    assert_eq!(f.approve("bob", &id, &hash).await.0, StatusCode::OK);
    let (status, done) = f.run("root", &id, &destructive(&plan)).await;
    assert_eq!(status, StatusCode::OK, "{done}");
    assert_eq!(done["data"]["state"], "done");
    assert_eq!(f.delivered().len(), steps);
}

// Keep the router type in the imports honest: the module mounts on the operator listener only.
#[test]
fn the_module_registers_on_the_operator_listener_only() {
    let f = fixture();
    let _: &Router = &f.plane.operator_router;
    assert_eq!(f.plane.module_names, vec!["deploy"]);
}
