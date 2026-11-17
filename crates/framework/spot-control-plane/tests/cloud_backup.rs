//! The `cloud_backup` module over the in-memory rows and object store: encrypted upload, resume,
//! digest verification, tenant isolation, quota, recovery code, retention and deletion; the S3
//! backend over a fake transport; a PostgreSQL smoke test through `SPOT_TEST_PG_URL` (prints why
//! it skips when unset).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, missing_docs)]

use std::cell::RefCell;
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use rcgen::KeyPair;
use serde_json::{Value, json};
use spot_cloud_backup::crypto::{DataKey, sha256_hex};
use spot_cloud_backup::{
    BackupManifest, ChunkEntry, MemoryObjectStore, ObjectStore, SourceInfo, chunk,
};
use spot_control_plane::auth::{KeyVault, sha256_hex as cp_sha256};
use spot_control_plane::config::Config;
use spot_control_plane::modules::cloud_backup::{self, CloudBackupState};
use spot_control_plane::modules::cloud_backup_s3::{
    S3Config, S3Request, S3Response, S3Store, S3Transport,
};
use spot_control_plane::modules::licensing::{self, LicensingState};
use spot_control_plane::modules::tenants_kv::{Kv, MemoryKv, PgKv};
use spot_control_plane::registry::Registration;
use spot_control_plane::{BuildDeps, ControlPlane, CpCtx, CpError};
use spot_core::{Clock, ManualClock};
use spot_mgmt_api::ports::{AuditSink, BoxFuture, MemoryAuditSink};
use spot_mgmt_api::tls::PeerCertificate;
use spot_signed_command::{Command, MemoryNonceStore, Signer, SoftwareSigner, sign};
use tower::ServiceExt as _;

const NOW: i64 = 1_800_000_000;
const DAY: u64 = 86_400;

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

thread_local! {
    static BACKUP: RefCell<Option<Arc<CloudBackupState>>> = const { RefCell::new(None) };
    static LICENCES: RefCell<Option<Arc<LicensingState>>> = const { RefCell::new(None) };
}

fn register_backup(ctx: &mut CpCtx) -> Result<(), CpError> {
    let state = BACKUP.with(|s| s.borrow().clone()).expect("state set");
    cloud_backup::register_with(ctx, state);
    Ok(())
}

fn register_licences(ctx: &mut CpCtx) -> Result<(), CpError> {
    let state = LICENCES.with(|s| s.borrow().clone()).expect("state set");
    licensing::register_with(ctx, state);
    Ok(())
}

struct Fixture {
    plane: ControlPlane,
    clock: Arc<ManualClock>,
    signer: SoftwareSigner,
    store: Arc<MemoryObjectStore>,
    state: Arc<CloudBackupState>,
}

async fn fixture_over(kv: Arc<dyn Kv>) -> Fixture {
    let signer = SoftwareSigner::from_seed(&[9u8; 32]).unwrap();
    let config = Config::from_toml(&config_text(&hex(&signer.public_key_bytes()))).unwrap();
    let clock = Arc::new(ManualClock::new(
        time::OffsetDateTime::from_unix_timestamp(NOW).unwrap(),
    ));
    let clock_dyn: Arc<dyn Clock> = clock.clone();
    let store = Arc::new(MemoryObjectStore::new());
    let state = CloudBackupState::new(Arc::clone(&kv), store.clone(), clock_dyn.clone());
    BACKUP.with(|s| *s.borrow_mut() = Some(Arc::clone(&state)));
    LICENCES.with(|s| {
        *s.borrow_mut() = Some(LicensingState::new(
            kv,
            clock_dyn.clone(),
            &[3u8; 32],
            "key-a",
        ));
    });
    let audit: Arc<dyn AuditSink> = Arc::new(MemoryAuditSink::new());
    let ca = KeyPair::generate().unwrap().serialize_pem();
    let plane = ControlPlane::build(BuildDeps {
        config: Arc::new(config),
        pool: None,
        clock: clock_dyn,
        audit,
        nonces: Arc::new(MemoryNonceStore::new()),
        vault: Arc::new(MemVault(ca)),
        registrations: vec![
            Registration {
                name: "licensing",
                order: licensing::ORDER,
                register: register_licences,
            },
            Registration {
                name: "cloud_backup",
                order: cloud_backup::ORDER,
                register: register_backup,
            },
        ],
    })
    .unwrap();
    let f = Fixture {
        plane,
        clock,
        signer,
        store,
        state,
    };
    for tenant in ["tenant-one", "tenant-two"] {
        let (status, issued) = f.licence(tenant, 2_000).await;
        assert_eq!(status, StatusCode::CREATED, "{issued}");
    }
    f
}

async fn fixture() -> Fixture {
    fixture_over(Arc::new(MemoryKv::new())).await
}

async fn answer(router: &axum::Router, req: Request<Body>) -> (StatusCode, Vec<u8>) {
    let response = router.clone().oneshot(req).await.unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 24)
        .await
        .unwrap();
    (status, bytes.to_vec())
}

fn as_json(bytes: &[u8]) -> Value {
    serde_json::from_slice(bytes).unwrap_or(Value::Null)
}

struct Backup {
    id: String,
    chunks: Vec<chunk::EncryptedChunk>,
    manifest: BackupManifest,
    artefact: Vec<u8>,
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
        if method != "GET" {
            let now = self.clock.now_utc().unix_timestamp();
            let command = Command::new(
                "cp.test",
                json!({ "body_sha256": cp_sha256(body.as_bytes()), "path": path }),
                "cp-eu-1",
                self.signer.key_id(),
                now - 5,
                now + 300,
            );
            let signed = sign(&command, &self.signer).unwrap();
            let text = spot_mgmt_api::wire::encode(&signed, &[]).unwrap();
            b = b.header("x-spot-command", URL_SAFE_NO_PAD.encode(text.as_bytes()));
        }
        let (status, bytes) = answer(
            &self.plane.operator_router,
            b.body(Body::from(body.to_owned())).unwrap(),
        )
        .await;
        (status, as_json(&bytes))
    }

    async fn licence(&self, tenant: &str, quota: u64) -> (StatusCode, Value) {
        let body = json!({ "tenant_id": tenant, "plan": "std", "starts_at": "2026-01-01T00:00:00Z",
            "ends_at": "2030-01-01T00:00:00Z", "retention_days": 30,
            "entitlements": ["cloud_backup", format!("cloud_backup_quota_bytes={quota}")] })
        .to_string();
        self.op("root", "POST", "/cp/v1/licences", &body).await
    }

    async fn host_raw(
        &self,
        tenant: &str,
        host: &str,
        method: &str,
        path: &str,
        headers: &[(&str, String)],
        body: Vec<u8>,
    ) -> (StatusCode, Vec<u8>) {
        let mut b = Request::builder()
            .method(method)
            .uri(path)
            .extension(PeerCertificate::new(
                &format!("CN=spot-host/{tenant}/{host}"),
                &format!("fp-{host}"),
            ));
        for (name, value) in headers {
            b = b.header(*name, value);
        }
        answer(&self.plane.host_router, b.body(Body::from(body)).unwrap()).await
    }

    async fn host(
        &self,
        tenant: &str,
        host: &str,
        method: &str,
        path: &str,
        body: &str,
    ) -> (StatusCode, Value) {
        let (status, bytes) = self
            .host_raw(tenant, host, method, path, &[], body.as_bytes().to_vec())
            .await;
        (status, as_json(&bytes))
    }

    fn prepare(&self, tenant: &str, id: &str, key: &DataKey) -> Backup {
        let artefact: Vec<u8> = (0..250u16)
            .map(|n| u8::try_from(n % 251).unwrap())
            .collect();
        let chunks = chunk::seal(id, key, &artefact, 100).unwrap();
        let entries = chunks
            .iter()
            .map(|c| ChunkEntry {
                index: c.index,
                offset: c.offset,
                plain_len: c.plain_len,
                cipher_len: c.cipher_len(),
                sha256: c.digest_hex(),
                object: BackupManifest::chunk_object(tenant, id, c.index, 0),
            })
            .collect();
        let manifest = BackupManifest::new(
            id,
            tenant,
            NOW,
            SourceInfo {
                name: "ledger.dump".to_owned(),
                bytes: 250,
                sha256: sha256_hex(&artefact),
            },
            100,
            entries,
            Vec::new(),
        );
        Backup {
            id: id.to_owned(),
            chunks,
            manifest,
            artefact,
        }
    }

    async fn begin(&self, host: &str, b: &Backup) -> (StatusCode, Value) {
        let cipher: u64 = b.chunks.iter().map(chunk::EncryptedChunk::cipher_len).sum();
        let body =
            json!({ "backup_id": b.id, "chunk_count": b.chunks.len(), "cipher_bytes": cipher });
        self.host(
            "tenant-one",
            host,
            "POST",
            "/cp/v1/host/backup/uploads",
            &body.to_string(),
        )
        .await
    }

    async fn put_chunk(&self, host: &str, b: &Backup, index: usize) -> (StatusCode, Value) {
        let c = &b.chunks[index];
        let (status, bytes) = self
            .host_raw(
                "tenant-one",
                host,
                "PUT",
                &format!("/cp/v1/host/backup/uploads/{}/chunks/{index}", b.id),
                &[("x-chunk-sha256", c.digest_hex())],
                c.bytes.clone(),
            )
            .await;
        (status, as_json(&bytes))
    }

    async fn commit(&self, host: &str, b: &Backup) -> (StatusCode, Value) {
        let path = format!("/cp/v1/host/backup/uploads/{}/commit", b.id);
        self.host(
            "tenant-one",
            host,
            "POST",
            &path,
            &b.manifest.to_json().unwrap(),
        )
        .await
    }

    async fn upload(&self, host: &str, id: &str, key: &DataKey) -> Backup {
        let b = self.prepare("tenant-one", id, key);
        assert_eq!(self.begin(host, &b).await.0, StatusCode::OK);
        for index in 0..b.chunks.len() {
            assert_eq!(self.put_chunk(host, &b, index).await.0, StatusCode::OK);
        }
        let (status, answer) = self.commit(host, &b).await;
        assert_eq!(status, StatusCode::OK, "{answer}");
        b
    }

    async fn listed(&self, host: &str) -> (StatusCode, Value) {
        self.host("tenant-one", host, "GET", "/cp/v1/host/backup/backups", "")
            .await
    }
}

#[tokio::test]
async fn upload_round_trip_stores_only_ciphertext_the_server_cannot_open() {
    let f = fixture().await;
    let key = DataKey::generate().unwrap();
    let b = f.upload("h1", "b1", &key).await;
    let (status, list) = f.listed("h1").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(list["data"].as_array().unwrap().len(), 1);
    let (_, manifest) = f
        .host(
            "tenant-one",
            "h1",
            "GET",
            "/cp/v1/host/backup/backups/b1/manifest",
            "",
        )
        .await;
    assert_eq!(manifest["data"]["backup_id"], "b1");
    let wrong = DataKey::generate().unwrap();
    for (index, original) in b.chunks.iter().enumerate() {
        let (status, bytes) = f
            .host_raw(
                "tenant-one",
                "h1",
                "GET",
                &format!("/cp/v1/host/backup/backups/b1/chunks/{index}"),
                &[],
                Vec::new(),
            )
            .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(bytes, original.bytes);
        let plain =
            chunk::open("b1", &key, original.index, &original.digest_hex(), &bytes).unwrap();
        let start = usize::try_from(original.offset).unwrap();
        assert_eq!(plain.as_slice(), &b.artefact[start..start + plain.len()]);
        assert!(chunk::open("b1", &wrong, original.index, &original.digest_hex(), &bytes).is_err());
    }
    // No stored object holds a plaintext run of the artefact.
    for object_key in f.store.keys() {
        let stored = f.store.get(&object_key).await.unwrap();
        assert!(
            !stored
                .windows(32)
                .any(|w| b.artefact.windows(32).any(|p| p == w))
        );
    }
}

#[tokio::test]
async fn an_interrupted_upload_resumes_and_an_incomplete_commit_is_refused() {
    let f = fixture().await;
    let key = DataKey::generate().unwrap();
    let b = f.prepare("tenant-one", "b1", &key);
    assert_eq!(f.begin("h1", &b).await.0, StatusCode::OK);
    assert_eq!(f.put_chunk("h1", &b, 0).await.1["data"]["stored"], true);
    assert_eq!(f.commit("h1", &b).await.0, StatusCode::CONFLICT);
    // The client resumes: the same begin reports what is there; a repeated chunk is a no-op.
    let (_, again) = f.begin("h1", &b).await;
    assert_eq!(again["data"]["received"], json!([0]));
    assert_eq!(f.put_chunk("h1", &b, 0).await.1["data"]["stored"], false);
    for index in 1..b.chunks.len() {
        f.put_chunk("h1", &b, index).await;
    }
    assert_eq!(f.commit("h1", &b).await.0, StatusCode::OK);
    // A committed backup is not begun again, and a different upload of the same id conflicts.
    assert_eq!(f.begin("h1", &b).await.0, StatusCode::CONFLICT);
}

#[tokio::test]
async fn a_chunk_whose_digest_does_not_match_is_refused() {
    let f = fixture().await;
    let key = DataKey::generate().unwrap();
    let b = f.prepare("tenant-one", "b1", &key);
    f.begin("h1", &b).await;
    let mut tampered = b.chunks[0].bytes.clone();
    tampered[0] ^= 1;
    let (status, _) = f
        .host_raw(
            "tenant-one",
            "h1",
            "PUT",
            "/cp/v1/host/backup/uploads/b1/chunks/0",
            &[("x-chunk-sha256", b.chunks[0].digest_hex())],
            tampered,
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(f.store.keys().is_empty());
}

#[tokio::test]
async fn another_tenant_cannot_list_or_read() {
    let f = fixture().await;
    let key = DataKey::generate().unwrap();
    f.upload("h1", "b1", &key).await;
    // tenant-two never backed up: no read at all without a recovery grant.
    let (status, _) = f
        .host(
            "tenant-two",
            "h9",
            "GET",
            "/cp/v1/host/backup/backups/b1/manifest",
            "",
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    // Once tenant-two backs up itself, tenant-one's backup id is simply not found for it.
    let body = json!({ "backup_id": "x", "chunk_count": 1, "cipher_bytes": 100 }).to_string();
    f.host(
        "tenant-two",
        "h9",
        "POST",
        "/cp/v1/host/backup/uploads",
        &body,
    )
    .await;
    let (status, _) = f
        .host(
            "tenant-two",
            "h9",
            "GET",
            "/cp/v1/host/backup/backups/b1/manifest",
            "",
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (_, list) = f
        .host("tenant-two", "h9", "GET", "/cp/v1/host/backup/backups", "")
        .await;
    assert!(list["data"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn the_quota_refuses_what_does_not_fit() {
    let f = fixture().await;
    let body = json!({ "backup_id": "big", "chunk_count": 10, "cipher_bytes": 5_000 }).to_string();
    let (status, answer) = f
        .host(
            "tenant-one",
            "h1",
            "POST",
            "/cp/v1/host/backup/uploads",
            &body,
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{answer}");
    assert_eq!(answer["error"]["code"], "cp.limit_exceeded");
    let key = DataKey::generate().unwrap();
    f.upload("h1", "b1", &key).await;
    let (_, usage) = f
        .host("tenant-one", "h1", "GET", "/cp/v1/host/backup/usage", "")
        .await;
    assert_eq!(usage["data"]["quota_bytes"], 2_000);
    assert!(usage["data"]["used_bytes"].as_u64().unwrap() > 250);
}

#[tokio::test]
async fn a_tenant_without_the_entitlement_is_refused() {
    let f = fixture().await;
    let body = json!({ "backup_id": "x", "chunk_count": 1, "cipher_bytes": 100 }).to_string();
    let (status, _) = f
        .host(
            "tenant-three",
            "h1",
            "POST",
            "/cp/v1/host/backup/uploads",
            &body,
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn the_recovery_code_is_single_use_and_a_new_pc_reads_only_with_its_grant() {
    let f = fixture().await;
    let key = DataKey::generate().unwrap();
    f.upload("h1", "b1", &key).await;
    // Only a host that backed up can issue the code; it is shown once.
    assert_eq!(
        f.host(
            "tenant-one",
            "h2",
            "POST",
            "/cp/v1/host/backup/recovery-code",
            ""
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    let (status, issued) = f
        .host(
            "tenant-one",
            "h1",
            "POST",
            "/cp/v1/host/backup/recovery-code",
            "",
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{issued}");
    let code = issued["data"]["recovery_code"].as_str().unwrap().to_owned();
    assert_eq!(
        f.host(
            "tenant-one",
            "h1",
            "POST",
            "/cp/v1/host/backup/recovery-code",
            ""
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    // The new PC (h2) cannot read, redeems the code and then can; the code works once.
    assert_eq!(f.listed("h2").await.0, StatusCode::FORBIDDEN);
    let redeem = json!({ "code": code }).to_string();
    let path = "/cp/v1/host/backup/recovery/redeem";
    assert_eq!(
        f.host("tenant-one", "h2", "POST", path, &redeem).await.0,
        StatusCode::OK
    );
    assert_eq!(f.listed("h2").await.0, StatusCode::OK);
    assert_eq!(
        f.host("tenant-one", "h3", "POST", path, &redeem).await.0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(f.listed("h3").await.0, StatusCode::FORBIDDEN);
    // The grant lasts an hour.
    f.clock.advance(std::time::Duration::from_secs(3_601));
    assert_eq!(f.listed("h2").await.0, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn wrong_recovery_codes_are_rate_limited() {
    let f = fixture().await;
    let key = DataKey::generate().unwrap();
    f.upload("h1", "b1", &key).await;
    let (_, issued) = f
        .host(
            "tenant-one",
            "h1",
            "POST",
            "/cp/v1/host/backup/recovery-code",
            "",
        )
        .await;
    let code = issued["data"]["recovery_code"].as_str().unwrap().to_owned();
    let path = "/cp/v1/host/backup/recovery/redeem";
    for _ in 0..5 {
        let wrong = json!({ "code": "spot-rc-nope" }).to_string();
        assert_eq!(
            f.host("tenant-one", "h2", "POST", path, &wrong).await.0,
            StatusCode::FORBIDDEN
        );
    }
    let right = json!({ "code": code }).to_string();
    assert_eq!(
        f.host("tenant-one", "h2", "POST", path, &right).await.0,
        StatusCode::TOO_MANY_REQUESTS
    );
}

#[tokio::test]
async fn retention_gates_deletion_and_the_purge_keeps_the_newest() {
    let f = fixture().await;
    let key = DataKey::generate().unwrap();
    f.upload("h1", "b1", &key).await;
    f.clock.advance(std::time::Duration::from_secs(10 * DAY));
    f.upload("h1", "b2", &key).await;
    let delete = |id: &str| format!("/cp/v1/host/backup/backups/{id}");
    assert_eq!(
        f.host("tenant-one", "h1", "DELETE", &delete("b1"), "")
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    f.clock.advance(std::time::Duration::from_secs(21 * DAY));
    assert_eq!(
        f.host("tenant-one", "h1", "DELETE", &delete("b1"), "")
            .await
            .0,
        StatusCode::OK
    );
    assert_eq!(
        f.host("tenant-one", "h1", "DELETE", &delete("b2"), "")
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    assert!(f.store.keys().iter().all(|k| !k.contains("/b1/")));
    // b2 is past retention but is the newest: the purge keeps it; a newer backup replaces it.
    f.clock.advance(std::time::Duration::from_secs(40 * DAY));
    assert_eq!(f.state.purge_expired().await.unwrap(), 0);
    f.upload("h1", "b3", &key).await;
    assert_eq!(f.state.purge_expired().await.unwrap(), 1);
    let (_, list) = f.listed("h1").await;
    assert_eq!(list["data"].as_array().unwrap().len(), 1);
    assert_eq!(list["data"][0]["backup_id"], "b3");
}

#[tokio::test]
async fn a_signed_operator_command_deletes_early_within_scope() {
    let f = fixture().await;
    let key = DataKey::generate().unwrap();
    f.upload("h1", "b1", &key).await;
    let reason = json!({ "reason": "customer request" }).to_string();
    let path = "/cp/v1/tenants/tenant-one/backups/b1/delete";
    assert_eq!(
        f.op(
            "alice",
            "POST",
            "/cp/v1/tenants/tenant-two/backups/b1/delete",
            &reason
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    let (status, answer) = f.op("alice", "POST", path, &reason).await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    assert!(f.store.keys().is_empty());
    assert!(
        f.listed("h1").await.1["data"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}

/// An in-memory S3: checks the headers the store signs and serves put/get/list/delete.
#[derive(Default)]
struct FakeS3 {
    objects: Mutex<std::collections::BTreeMap<String, Vec<u8>>>,
    seen: Mutex<Vec<S3Request>>,
}

impl S3Transport for FakeS3 {
    fn send<'a>(&'a self, request: S3Request) -> BoxFuture<'a, Result<S3Response, String>> {
        self.seen.lock().unwrap().push(request.clone());
        let header = |name: &str| {
            request
                .headers
                .iter()
                .find(|(n, _)| n == name)
                .map(|(_, v)| v.clone())
        };
        assert_eq!(header("host").as_deref(), Some("s3.test"));
        assert_eq!(
            header("x-amz-content-sha256"),
            Some(sha256_hex(&request.body))
        );
        let auth = header("authorization").expect("signed");
        assert!(
            auth.starts_with("AWS4-HMAC-SHA256 Credential=AKID/"),
            "{auth}"
        );
        assert!(auth.contains("/eu-1/s3/aws4_request"), "{auth}");
        assert!(auth.contains("SignedHeaders=host;"), "{auth}");
        let (path, query) = request.path.split_once('?').unwrap_or((&request.path, ""));
        let mut objects = self.objects.lock().unwrap();
        let ok = |status: u16, body: Vec<u8>| -> Result<S3Response, String> {
            Ok(S3Response { status, body })
        };
        let result = match request.method {
            "PUT" => {
                if objects.contains_key(path) {
                    ok(412, Vec::new())
                } else {
                    assert_eq!(header("if-none-match").as_deref(), Some("*"));
                    objects.insert(path.to_owned(), request.body.clone());
                    ok(200, Vec::new())
                }
            }
            "GET" if path == "/bkt" => {
                let prefix = query
                    .split('&')
                    .find_map(|p| p.strip_prefix("prefix="))
                    .unwrap_or("")
                    .replace("%2F", "/");
                let want = format!("/bkt/{prefix}");
                let mut xml = String::from("<ListBucketResult><IsTruncated>false</IsTruncated>");
                for (key, value) in objects.iter().filter(|(k, _)| k.starts_with(&want)) {
                    xml.push_str(&format!(
                        "<Contents><Key>{}</Key><Size>{}</Size></Contents>",
                        &key["/bkt/".len()..],
                        value.len()
                    ));
                }
                xml.push_str("</ListBucketResult>");
                ok(200, xml.into_bytes())
            }
            "GET" => match objects.get(path) {
                Some(bytes) => ok(200, bytes.clone()),
                None => ok(404, Vec::new()),
            },
            "DELETE" => {
                objects.remove(path);
                ok(204, Vec::new())
            }
            _ => ok(400, Vec::new()),
        };
        Box::pin(std::future::ready(result))
    }
}

#[tokio::test]
async fn the_s3_store_signs_requests_and_keeps_objects_immutable() {
    let transport = Arc::new(FakeS3::default());
    let clock: Arc<dyn Clock> = Arc::new(ManualClock::new(
        time::OffsetDateTime::from_unix_timestamp(NOW).unwrap(),
    ));
    let config = S3Config::new("s3.test", "bkt", "eu-1", "AKID", "very-secret-key");
    assert!(!format!("{config:?}").contains("very-secret-key"));
    let store = S3Store::new(transport.clone(), config, clock);
    use spot_cloud_backup::PutOutcome;
    assert_eq!(
        store
            .put("t1/b1/chunk-0.bin", b"abc".to_vec())
            .await
            .unwrap(),
        PutOutcome::Stored
    );
    assert_eq!(
        store
            .put("t1/b1/chunk-0.bin", b"zzz".to_vec())
            .await
            .unwrap(),
        PutOutcome::Exists
    );
    assert_eq!(store.get("t1/b1/chunk-0.bin").await.unwrap(), b"abc");
    store
        .put("t1/b2/chunk-0.bin", b"de".to_vec())
        .await
        .unwrap();
    store.put("t2/b1/chunk-0.bin", b"f".to_vec()).await.unwrap();
    let listed = store.list("t1/").await.unwrap();
    assert_eq!(listed.len(), 2);
    assert_eq!(listed.iter().map(|o| o.bytes).sum::<u64>(), 5);
    store.delete("t1/b1/chunk-0.bin").await.unwrap();
    assert!(store.get("t1/b1/chunk-0.bin").await.is_err());
    // The secret never reaches the wire.
    for request in transport.seen.lock().unwrap().iter() {
        assert!(
            !request
                .headers
                .iter()
                .any(|(_, v)| v.contains("very-secret-key"))
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn upload_over_postgresql() {
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
        "DELETE FROM cp_tenants_kv WHERE ns LIKE 'bk\\_%' OR ns LIKE 'licence\\_%'",
    ] {
        client.batch_execute(statement).await.unwrap();
    }
    drop(client);
    let f = fixture_over(Arc::new(PgKv::new(pool))).await;
    let key = DataKey::generate().unwrap();
    f.upload("h1", "b1", &key).await;
    assert_eq!(f.listed("h1").await.1["data"].as_array().unwrap().len(), 1);
}
