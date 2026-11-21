//! The `tenants` module: provisioning, idempotency, crash-resume at every step, scope, the
//! edition limit under concurrency and the two-step deprovisioning. The workflow runs over the
//! in-memory store and fake ports; the PostgreSQL rows run through `SPOT_TEST_PG_URL` and print
//! why they skip when it is unset.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, missing_docs)]

use std::cell::RefCell;
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use rcgen::KeyPair;
use serde_json::{Value, json};
use spot_control_plane::auth::{KeyVault, sha256_hex};
use spot_control_plane::config::Config;
use spot_control_plane::modules::tenants::{
    self, Deps, Kv, MemoryKv, PortSet, SignedApprovals, TenantsState,
};
use spot_control_plane::registry::Registration;
use spot_control_plane::{BuildDeps, ControlPlane, CpCtx, CpError};
use spot_core::{Clock, ManualClock};
use spot_mgmt_api::ports::{AuditSink, MemoryAuditSink};
use spot_mgmt_api::tls::PeerCertificate;
use spot_signed_command::{Command, MemoryNonceStore, Signer, SoftwareSigner, sign};
use spot_tenancy::model::ClusterId;
use spot_tenancy::ports::{
    Archiver, AuditEvent, AuditSink as PlaneAudit, IamPort, LicencePort, PortResult, SchemaPort,
    SiteSeeder, StepFailure, TelemetryPort,
};
use spot_tenancy::{Cluster, DatabaseId, MemoryStore, Region, Tenant};
use tower::ServiceExt as _;

const NOW: i64 = 1_800_000_000;
const DAY_MS: u64 = 86_400_000;

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
tenants = ["00000000-0000-7000-8000-000000000000"]
"#
    )
}

/// Records every port call and can fail one step once, after its effect landed.
#[derive(Default)]
struct Fake {
    calls: Mutex<Vec<String>>,
    fail_after: Mutex<Option<&'static str>>,
}

impl Fake {
    fn step(&self, name: &'static str) -> PortResult {
        self.calls.lock().unwrap().push(name.to_owned());
        let mut slot = self.fail_after.lock().unwrap();
        if *slot == Some(name) {
            *slot = None;
            return Err(StepFailure {
                retryable: true,
                reason: "fake.killed",
            });
        }
        Ok(())
    }
    fn count(&self, name: &str) -> usize {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|c| *c == name)
            .count()
    }
}

impl SchemaPort for Fake {
    fn create(&self, _: &Tenant) -> PortResult {
        self.step("create_schema")
    }
    fn drop_schema(&self, _: &Tenant) -> PortResult {
        self.step("drop_schema")
    }
}
impl SiteSeeder for Fake {
    fn seed(&self, _: &Tenant) -> PortResult {
        self.step("seed")
    }
    fn unseed(&self, _: &Tenant) -> PortResult {
        self.step("unseed")
    }
}
impl IamPort for Fake {
    fn create_users(&self, _: &Tenant) -> PortResult {
        self.step("create_users")
    }
    fn remove_users(&self, _: &Tenant) -> PortResult {
        self.step("remove_users")
    }
}
impl LicencePort for Fake {
    fn issue(&self, _: &Tenant) -> PortResult {
        self.step("issue_licence")
    }
    fn revoke(&self, _: &Tenant) -> PortResult {
        self.step("revoke_licence")
    }
}
impl TelemetryPort for Fake {
    fn register(&self, _: &Tenant) -> PortResult {
        self.step("register_telemetry")
    }
    fn deregister(&self, _: &Tenant) -> PortResult {
        self.step("deregister_telemetry")
    }
}
impl Archiver for Fake {
    fn final_backup(&self, _: &Tenant) -> PortResult<String> {
        self.step("final_backup").map(|()| "backup-1".to_owned())
    }
    fn verify(&self, _: &Tenant, _: &str) -> PortResult<bool> {
        Ok(true)
    }
}

struct NullAudit;
impl PlaneAudit for NullAudit {
    fn record(&self, _: &AuditEvent) -> PortResult {
        Ok(())
    }
}

thread_local! {
    static STATE: RefCell<Option<Arc<TenantsState>>> = const { RefCell::new(None) };
}

fn register(ctx: &mut CpCtx) -> Result<(), CpError> {
    let state = STATE.with(|s| s.borrow().clone()).expect("state set");
    tenants::register_with(ctx, state);
    Ok(())
}

struct Fixture {
    plane: ControlPlane,
    state: Arc<TenantsState>,
    fake: Arc<Fake>,
    clock: Arc<ManualClock>,
    signer: SoftwareSigner,
    audit: Arc<MemoryAuditSink>,
}

fn fixture(editions: &[(&str, u32)], capacity: u32) -> Fixture {
    let signer = SoftwareSigner::from_seed(&[9u8; 32]).unwrap();
    let config = Config::from_toml(&config_text(&hex(&signer.public_key_bytes()))).unwrap();
    let clock = Arc::new(ManualClock::new(
        time::OffsetDateTime::from_unix_timestamp(NOW).unwrap(),
    ));
    let fake = Arc::new(Fake::default());
    let counter = std::sync::atomic::AtomicU8::new(0);
    let deps = Deps {
        store: Arc::new(MemoryStore::new()),
        ports: PortSet {
            schema: fake.clone(),
            seeder: fake.clone(),
            iam: fake.clone(),
            licence: fake.clone(),
            telemetry: fake.clone(),
            archiver: fake.clone(),
        },
        audit: Arc::new(NullAudit),
        kv: Arc::new(MemoryKv::new()),
        approvals: Arc::new(SignedApprovals::new(
            &[signer.public_key_bytes()],
            Box::new(MemoryNonceStore::new()),
            "cp-eu-1",
        )),
        clusters: vec![Cluster {
            cluster: ClusterId::new("c1").unwrap(),
            database: DatabaseId::new("d1").unwrap(),
            region: Region::new("eu-1").unwrap(),
            capacity,
            accepting: true,
        }],
        plane: spot_tenancy::Config::default(),
        editions: editions
            .iter()
            .map(|(k, v)| ((*k).to_owned(), *v))
            .collect(),
        confirm_delay_ms: DAY_MS,
        retention_ms: 7 * DAY_MS,
        entropy: Arc::new(move || [counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst); 10]),
        poll_ms: 10,
        max_attempts: 5,
    };
    let clock_dyn: Arc<dyn Clock> = clock.clone();
    let state = TenantsState::ready(clock_dyn.clone(), deps);
    STATE.with(|s| *s.borrow_mut() = Some(Arc::clone(&state)));
    let ca = KeyPair::generate().unwrap().serialize_pem();
    let audit = Arc::new(MemoryAuditSink::new());
    let audit_dyn: Arc<dyn AuditSink> = audit.clone();
    let plane = ControlPlane::build(BuildDeps {
        config: Arc::new(config),
        pool: None,
        clock: clock_dyn,
        audit: audit_dyn,
        nonces: Arc::new(MemoryNonceStore::new()),
        vault: Arc::new(MemVault(ca)),
        registrations: vec![Registration {
            name: "tenants",
            order: tenants::ORDER,
            register,
        }],
    })
    .unwrap();
    Fixture {
        plane,
        state,
        fake,
        clock,
        signer,
        audit,
    }
}

impl Fixture {
    fn now(&self) -> i64 {
        self.clock.now_utc().unix_timestamp()
    }

    fn header(&self, op: &str, path: &str, body: &str) -> String {
        let command = Command::new(
            op,
            json!({ "body_sha256": sha256_hex(body.as_bytes()), "path": path }),
            "cp-eu-1",
            self.signer.key_id(),
            self.now() - 5,
            self.now() + 300,
        );
        let signed = sign(&command, &self.signer).unwrap();
        let text = spot_mgmt_api::wire::encode(&signed, &[]).unwrap();
        URL_SAFE_NO_PAD.encode(text.as_bytes())
    }

    fn approval(&self, tenant: &str, request_id: &str) -> String {
        self.header(
            "cp.cp_tenant_deprovision_approve",
            &format!("/cp/v1/tenants/{tenant}/deprovision/confirm"),
            &format!("{tenant}:{request_id}"),
        )
    }

    /// A signed mutation (or a plain read when `signed` is false).
    async fn send(
        &self,
        who: &str,
        method: &str,
        path: &str,
        body: &str,
        key: Option<&str>,
    ) -> (StatusCode, Value) {
        let mut builder =
            Request::builder()
                .method(method)
                .uri(path)
                .extension(PeerCertificate::new(
                    &format!("CN={who}"),
                    &format!("fp-{who}"),
                ));
        if method == "POST" {
            builder = builder.header("x-spot-command", self.header("cp.test", path, body));
        }
        if let Some(key) = key {
            builder = builder.header("idempotency-key", key);
        }
        let response = self
            .plane
            .operator_router
            .clone()
            .oneshot(builder.body(Body::from(body.to_owned())).unwrap())
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

    async fn create(&self, name: &str, plan: &str, key: &str) -> (StatusCode, Value) {
        let body = json!({ "name": name, "region": "eu-1", "plan": plan, "admin_email": "a@example.test" }).to_string();
        self.send("root", "POST", "/cp/v1/tenants", &body, Some(key))
            .await
    }

    async fn tenant(&self, id: &str) -> Value {
        self.send("root", "GET", &format!("/cp/v1/tenants/{id}"), "", None)
            .await
            .1
    }
}

fn id_of(answer: &Value) -> String {
    answer["data"]["tenant"]["id"].as_str().unwrap().to_owned()
}

#[tokio::test]
async fn provisioning_registers_then_the_runner_activates_and_the_key_is_idempotent() {
    let f = fixture(&[], 10);
    let (status, first) = f.create("acme-one", "std", "k1").await;
    assert_eq!(status, StatusCode::ACCEPTED, "{first}");
    let id = id_of(&first);
    assert_eq!(first["data"]["tenant"]["status"], "provisioning");
    assert_eq!(
        f.fake.count("create_schema"),
        0,
        "the request creates nothing yet"
    );
    assert_eq!(tenants::run_once(&f.state).await, 1);
    let tenant = f.tenant(&id).await;
    assert_eq!(tenant["data"]["status"], "active", "{tenant}");
    // Same key, same body: the first answer. Same key, other body: replayed.
    let (status, again) = f.create("acme-one", "std", "k1").await;
    assert_eq!((status, id_of(&again)), (StatusCode::ACCEPTED, id.clone()));
    let (status, other) = f.create("acme-two", "std", "k1").await;
    assert_eq!(
        (status, other["error"]["code"].as_str()),
        (StatusCode::CONFLICT, Some("cp.replayed"))
    );
    // A missing key is refused.
    let body = json!({ "name": "acme-three", "region": "eu-1", "plan": "std", "admin_email": "a@example.test" }).to_string();
    let (status, _) = f.send("root", "POST", "/cp/v1/tenants", &body, None).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    // The workflow shows every step done.
    let (_, wf) = f
        .send(
            "root",
            "GET",
            &format!("/cp/v1/workflows/wf-p-{id}"),
            "",
            None,
        )
        .await;
    assert_eq!(wf["data"]["state"], "succeeded", "{wf}");
    assert_eq!(wf["data"]["steps"].as_array().unwrap().len(), 6);
    // Lists, placements and clusters see it.
    let (_, list) = f.send("root", "GET", "/cp/v1/tenants", "", None).await;
    assert_eq!(list["data"].as_array().unwrap().len(), 1);
    let (_, placements) = f.send("root", "GET", "/cp/v1/placements", "", None).await;
    assert_eq!(placements["data"][0]["cluster"], "c1");
    let (_, clusters) = f.send("root", "GET", "/cp/v1/clusters", "", None).await;
    assert_eq!(
        (
            clusters["data"][0]["tenants"].as_u64(),
            clusters["data"][0]["state"].as_str()
        ),
        (Some(1), Some("green"))
    );
    assert!(!f.audit.records().is_empty());
}

#[tokio::test]
async fn a_runner_killed_after_each_step_resumes_to_the_same_end_state() {
    for step in [
        "create_schema",
        "seed",
        "create_users",
        "issue_licence",
        "register_telemetry",
    ] {
        let f = fixture(&[], 10);
        *f.fake.fail_after.lock().unwrap() = Some(step);
        let (_, created) = f.create("acme-one", "std", "k1").await;
        let id = id_of(&created);
        tenants::run_once(&f.state).await;
        let (_, wf) = f
            .send(
                "root",
                "GET",
                &format!("/cp/v1/workflows/wf-p-{id}"),
                "",
                None,
            )
            .await;
        assert_eq!(wf["data"]["state"], "failed", "{step}: {wf}");
        assert_eq!(wf["data"]["resumable"], true);
        tenants::run_once(&f.state).await;
        assert_eq!(f.tenant(&id).await["data"]["status"], "active", "{step}");
        for name in [
            "create_schema",
            "seed",
            "create_users",
            "issue_licence",
            "register_telemetry",
        ] {
            assert!(f.fake.count(name) >= 1, "{step}: {name} never ran");
        }
    }
}

#[tokio::test]
async fn scope_hides_other_tenants_and_forbids_creating() {
    let f = fixture(&[], 10);
    let (_, created) = f.create("acme-one", "std", "k1").await;
    let id = id_of(&created);
    let (status, body) = f
        .send("alice", "GET", &format!("/cp/v1/tenants/{id}"), "", None)
        .await;
    assert_eq!(
        (status, body["error"]["code"].as_str()),
        (StatusCode::NOT_FOUND, Some("cp.not_found"))
    );
    let (status, _) = f
        .send(
            "alice",
            "GET",
            &format!("/cp/v1/workflows/wf-p-{id}"),
            "",
            None,
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (_, list) = f.send("alice", "GET", "/cp/v1/tenants", "", None).await;
    assert!(list["data"].as_array().unwrap().is_empty());
    let body = json!({ "name": "acme-two", "region": "eu-1", "plan": "std", "admin_email": "a@example.test" }).to_string();
    let (status, _) = f
        .send("alice", "POST", "/cp/v1/tenants", &body, Some("k9"))
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = f
        .send("alice", "GET", "/cp/v1/tenants/not-an-id", "", None)
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn the_edition_limit_admits_one_of_two_parallel_creates() {
    let f = fixture(&[("std", 1)], 10);
    let (a, b) = tokio::join!(
        f.create("acme-one", "std", "ka"),
        f.create("acme-two", "std", "kb")
    );
    let mut statuses = [a.0.as_u16(), b.0.as_u16()];
    statuses.sort_unstable();
    assert_eq!(statuses, [202, 403], "{a:?} {b:?}");
    let denied = if a.0 == StatusCode::FORBIDDEN {
        &a.1
    } else {
        &b.1
    };
    assert_eq!(denied["error"]["code"], "cp.limit_exceeded");
}

#[tokio::test]
async fn deprovisioning_needs_two_steps_a_signed_approval_and_the_retention_to_pass() {
    let f = fixture(&[], 10);
    let (_, created) = f.create("acme-one", "std", "k1").await;
    let id = id_of(&created);
    tenants::run_once(&f.state).await;
    // Not suspended: refused.
    let path = format!("/cp/v1/tenants/{id}/deprovision");
    let reason = json!({ "reason": "contract ended" }).to_string();
    let (status, _) = f.send("root", "POST", &path, &reason, Some("d1")).await;
    assert_eq!(status, StatusCode::CONFLICT);
    let (status, _) = f
        .send(
            "root",
            "POST",
            &format!("/cp/v1/tenants/{id}/suspend"),
            "",
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let (status, request) = f.send("root", "POST", &path, &reason, Some("d2")).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{request}");
    let request_id = request["data"]["request_id"].as_str().unwrap().to_owned();
    assert_eq!(f.fake.count("drop_schema"), 0);
    // Confirm too early, then after the delay but with a bad approval.
    let confirm_path = format!("{path}/confirm");
    let body =
        |approval: &str| json!({ "request_id": request_id, "approval": approval }).to_string();
    let (status, early) = f
        .send("bob", "POST", &confirm_path, &body("x"), Some("c1"))
        .await;
    assert_eq!(
        (status, early["error"]["code"].as_str()),
        (StatusCode::CONFLICT, Some("cp.not_confirmable"))
    );
    f.clock.advance(std::time::Duration::from_secs(86_400 + 10));
    let (status, bad) = f
        .send(
            "bob",
            "POST",
            &confirm_path,
            &body("not-an-approval"),
            Some("c2"),
        )
        .await;
    assert_eq!(
        (status, bad["error"]["code"].as_str()),
        (StatusCode::UNAUTHORIZED, Some("cp.signature_invalid"))
    );
    // A valid approval inside the retention: accepted, waiting, nothing deleted.
    let good = body(&f.approval(&id, &request_id));
    let (status, waiting) = f
        .send("bob", "POST", &confirm_path, &good, Some("c3"))
        .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{waiting}");
    assert_eq!(waiting["data"]["state"], "waiting");
    assert_eq!(
        f.fake.count("drop_schema"),
        0,
        "no data deleted before the retention ends"
    );
    tenants::run_once(&f.state).await;
    assert_eq!(f.fake.count("drop_schema"), 0);
    // After the retention the runner deletes.
    f.clock.advance(std::time::Duration::from_secs(7 * 86_400));
    tenants::run_once(&f.state).await;
    assert_eq!(f.fake.count("drop_schema"), 1);
    assert_eq!(f.tenant(&id).await["data"]["status"], "deprovisioned");
    let (_, wf) = f
        .send(
            "root",
            "GET",
            &format!("/cp/v1/workflows/wf-d-{id}"),
            "",
            None,
        )
        .await;
    assert_eq!(wf["data"]["state"], "succeeded", "{wf}");
}

#[tokio::test]
async fn the_requester_cannot_also_confirm_alone() {
    let f = fixture(&[], 10);
    let (_, created) = f.create("acme-one", "std", "k1").await;
    let id = id_of(&created);
    tenants::run_once(&f.state).await;
    f.send(
        "root",
        "POST",
        &format!("/cp/v1/tenants/{id}/suspend"),
        "",
        None,
    )
    .await;
    let path = format!("/cp/v1/tenants/{id}/deprovision");
    let (_, request) = f
        .send(
            "root",
            "POST",
            &path,
            &json!({ "reason": "x" }).to_string(),
            Some("d"),
        )
        .await;
    let request_id = request["data"]["request_id"].as_str().unwrap().to_owned();
    f.clock.advance(std::time::Duration::from_secs(8 * 86_400));
    let body =
        json!({ "request_id": request_id, "approval": f.approval(&id, &request_id) }).to_string();
    // root asked and root confirms: only the key remains as approver, which the plane refuses.
    let (status, answer) = f
        .send("root", "POST", &format!("{path}/confirm"), &body, Some("c"))
        .await;
    assert_eq!(
        (status, answer["error"]["code"].as_str()),
        (StatusCode::FORBIDDEN, Some("cp.approval_required")),
        "{answer}"
    );
    assert_eq!(f.fake.count("drop_schema"), 0);
}

#[tokio::test]
async fn the_memory_kv_gives_a_lease_to_one_holder_and_creates_once() {
    let kv = MemoryKv::new();
    assert!(kv.put_new("ns", "k", "1").await.unwrap());
    assert!(!kv.put_new("ns", "k", "2").await.unwrap());
    assert!(kv.lease("w", "a", 0, 100).await.unwrap());
    assert!(!kv.lease("w", "b", 50, 100).await.unwrap());
    assert!(kv.lease("w", "b", 200, 100).await.unwrap());
}

#[tokio::test]
async fn postgres_kv_rows_leases_and_gates_behave_like_the_memory_twin() {
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
        "DELETE FROM cp_tenants_kv WHERE ns = 'pgtest'",
        "DELETE FROM cp_tenants_lease WHERE id = 'pgtest-w'",
    ] {
        client.batch_execute(statement).await.unwrap();
    }
    drop(client);
    let kv = tenants::PgKv::new(pool);
    assert!(kv.put_new("pgtest", "k", "1").await.unwrap());
    assert!(!kv.put_new("pgtest", "k", "2").await.unwrap());
    assert_eq!(kv.get("pgtest", "k").await.unwrap().as_deref(), Some("1"));
    assert!(kv.lease("pgtest-w", "a", 0, 100).await.unwrap());
    assert!(!kv.lease("pgtest-w", "b", 50, 100).await.unwrap());
    assert!(kv.lease("pgtest-w", "b", 200, 100).await.unwrap());
    let gate = kv.gate("pgtest-gate").await.unwrap();
    kv.ungate(gate).await;
}
