//! The `cloud_backup` module (`WP-CTL-08.T2`): the service `spot-cloud-backup` talks to.
//!
//! Host endpoints under `/cp/v1/host/backup/` (the tenant is always the certificate's, never a
//! body field; objects live under the tenant's own prefix `<tenant>/<backup>/`):
//!
//! - `POST uploads` begins (or resumes) an upload: the licence must be current and carry the
//!   `cloud_backup` entitlement (`spot_cloud_backup::TenantAuth`), the declared bytes must fit the
//!   quota (`cloud_backup_quota_bytes=<n>` entitlement, default 10 GiB) over the usage the module
//!   counts itself. `PUT uploads/{backup}/chunks/{index}` takes one ciphertext chunk: its
//!   SHA-256 must equal `x-chunk-sha256`, a stored chunk is immutable (the same bytes again are
//!   a no-op, other bytes conflict), the chunk rows are per chunk so parallel and repeated uploads
//!   resume cleanly. `POST uploads/{backup}/commit` takes the client's manifest, checks it against
//!   the received chunks (count, bytes, digests, object keys) and stores it verbatim.
//! - `GET backups`, `.../{id}/manifest`, `.../{id}/chunks/{index}`, `GET usage` read; a host that
//!   never backed up (a new PC) reads only with a grant from `POST recovery/redeem`.
//! - `POST recovery-code` issues the tenant's one-time recovery code (128-bit random, shown once,
//!   stored as a digest); `POST recovery/redeem` spends it (single use, 5 wrong tries per hour
//!   per tenant, then `cp.rate_limited`) and grants the calling host one hour of reading.
//! - `DELETE backups/{id}` works only after the licence's retention (days, default 30) passed;
//!   earlier deletion is the signed operator command `POST /cp/v1/tenants/{id}/backups/{backup}/delete`.
//!   A timer purges backups past retention, always keeping the newest.
//!
//! The service stores ciphertext and the client's wrapped keys only; it never has a data key and
//! no code path here decrypts. Backends are `spot_cloud_backup::ObjectStore` implementations: the
//! filesystem store of that crate and [`super::cloud_backup_s3::S3Store`].

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path, Query, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse as _, Response};
use axum::routing::{delete, get, post, put};
use axum::{Extension, Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use spot_cloud_backup::crypto::{random_hex, sha256_hex};
use spot_cloud_backup::{BackupManifest, BackupState, CloudBackupError, ObjectStore, TenantAuth};
use spot_core::{Clock, SiteId};
use spot_license::SignedLicence;

use super::tenants_kv::{Kv, PgKv, from_json, to_json};
use super::tenants_state::finish;
use crate::auth::{HostIdentity, OperatorCall, RequestId};
use crate::{CpCode, CpCtx, CpError};

/// Registration order: clear of the other modules.
pub const ORDER: u32 = 190;

const DDL: &[&str] = &[
    "CREATE TABLE IF NOT EXISTS cp_tenants_kv (ns TEXT NOT NULL, k TEXT NOT NULL, v TEXT NOT NULL, PRIMARY KEY (ns, k))",
];
/// The default storage quota when the licence names none: 10 GiB.
pub const DEFAULT_QUOTA_BYTES: u64 = 10 * 1024 * 1024 * 1024;
const DEFAULT_RETENTION_DAYS: u64 = 30;
const MAX_CHUNK_BODY: usize = 16 * 1024 * 1024;
const MAX_MANIFEST_BODY: usize = 4 * 1024 * 1024;
const MAX_CHUNKS: u32 = 100_000;
const RECOVERY_FAILS: u32 = 5;
const RECOVERY_WINDOW_SECS: i64 = 3_600;
const GRANT_SECS: i64 = 3_600;
const PURGE_EVERY_SECS: u64 = 3_600;
const QUOTA_PREFIX: &str = "cloud_backup_quota_bytes=";

/// The state of the module.
pub struct CloudBackupState {
    kv: Arc<dyn Kv>,
    store: Arc<dyn ObjectStore>,
    clock: Arc<dyn Clock>,
}

impl CloudBackupState {
    /// A state over the control rows `kv` and the object `store`.
    #[must_use]
    pub fn new(kv: Arc<dyn Kv>, store: Arc<dyn ObjectStore>, clock: Arc<dyn Clock>) -> Arc<Self> {
        Arc::new(Self { kv, store, clock })
    }

    fn now(&self) -> i64 {
        self.clock.now_utc().unix_timestamp()
    }
}

#[derive(Clone, Serialize, Deserialize)]
struct UploadRec {
    chunk_count: u32,
    cipher_bytes: u64,
    committed: bool,
}

#[derive(Clone, Serialize, Deserialize)]
struct ChunkRec {
    digest: String,
    len: u64,
    object: String,
}

#[derive(Clone, Serialize, Deserialize)]
struct BackupRec {
    committed_at: i64,
    cipher_bytes: u64,
    manifest_key: String,
}

#[derive(Serialize, Deserialize)]
struct CodeRec {
    digest: String,
    issued_at: i64,
}

#[derive(Default, Serialize, Deserialize)]
struct RateRec {
    start: i64,
    fails: u32,
}

fn backup_error(error: &CloudBackupError) -> CpError {
    match error {
        CloudBackupError::NotEntitled { .. } => {
            CpError::with_reason(CpCode::Forbidden, "backup.not_entitled")
        }
        _ => CpError::with_reason(CpCode::Internal, "backup.store"),
    }
}

fn store_error(error: &CloudBackupError) -> CpError {
    let _ = error;
    CpError::with_reason(CpCode::Internal, "backup.store")
}

fn valid_segment(text: &str) -> bool {
    !text.is_empty()
        && text.len() <= 128
        && text != "."
        && text != ".."
        && text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

fn hex64(text: &str) -> bool {
    text.len() == 64 && text.bytes().all(|b| b.is_ascii_hexdigit())
}

fn checked_segment(text: &str) -> Result<(), CpError> {
    if valid_segment(text) {
        Ok(())
    } else {
        Err(CpError::invalid(&["backup_id"]))
    }
}

impl CloudBackupState {
    /// The licence facts of a tenant: the entitlement check, the quota and the retention.
    async fn licence(&self, tenant: &str) -> Result<(u64, u64), CpError> {
        let now = self.now();
        let denied = || CpError::with_reason(CpCode::Forbidden, "backup.not_entitled");
        if self.kv.get("licence_revoked", tenant).await?.is_some() {
            return Err(denied());
        }
        let text = self
            .kv
            .get("licence_active", tenant)
            .await?
            .ok_or_else(denied)?;
        let licence = SignedLicence::from_text(&text)
            .and_then(|signed| signed.claimed_licence())
            .map_err(|_| denied())?;
        let site = SiteId::new(tenant).map_err(|_| denied())?;
        TenantAuth::new(site, licence, now).map_err(|e| backup_error(&e))?;
        let meta: Value = match self.kv.get("licence_meta", tenant).await? {
            Some(text) => from_json(&text)?,
            None => Value::Null,
        };
        let entitled = |prefix: &str| {
            meta["entitlements"].as_array().and_then(|list| {
                list.iter()
                    .filter_map(Value::as_str)
                    .find_map(|e| e.strip_prefix(prefix)?.parse::<u64>().ok())
            })
        };
        let quota = entitled(QUOTA_PREFIX).unwrap_or(DEFAULT_QUOTA_BYTES);
        let days = meta["retention_days"]
            .as_u64()
            .unwrap_or(DEFAULT_RETENTION_DAYS);
        Ok((quota, days))
    }

    async fn chunks(&self, tenant: &str, backup: &str) -> Result<Vec<(u32, ChunkRec)>, CpError> {
        let prefix = format!("{tenant}/{backup}/");
        let mut out = Vec::new();
        for (key, text) in self.kv.list("bk_chunk").await? {
            if let Some(index) = key.strip_prefix(&prefix) {
                out.push((
                    index.parse().unwrap_or(u32::MAX),
                    from_json::<ChunkRec>(&text)?,
                ));
            }
        }
        Ok(out)
    }

    async fn used(&self, tenant: &str) -> Result<u64, CpError> {
        let prefix = format!("{tenant}/");
        let mut total = 0u64;
        for (key, text) in self.kv.list("bk_chunk").await? {
            if key.starts_with(&prefix) {
                total = total.saturating_add(from_json::<ChunkRec>(&text)?.len);
            }
        }
        Ok(total)
    }

    async fn may_read(&self, host: &HostIdentity) -> Result<(), CpError> {
        let key = format!("{}/{}", host.tenant, host.host_id);
        if self.kv.get("bk_host", &key).await?.is_some() {
            return Ok(());
        }
        let granted = self
            .kv
            .get("bk_grant", &key)
            .await?
            .and_then(|t| t.parse::<i64>().ok())
            .is_some_and(|until| self.now() < until);
        if granted {
            Ok(())
        } else {
            Err(CpError::with_reason(
                CpCode::Forbidden,
                "backup.recovery_required",
            ))
        }
    }

    async fn backup(&self, tenant: &str, id: &str) -> Result<BackupRec, CpError> {
        let text = self
            .kv
            .get("bk_backup", &format!("{tenant}/{id}"))
            .await?
            .ok_or_else(|| CpError::new(CpCode::NotFound))?;
        from_json(&text)
    }

    /// Removes a backup: its objects, chunk rows and bookkeeping.
    async fn remove(&self, tenant: &str, id: &str) -> Result<(), CpError> {
        let prefix = format!("{tenant}/{id}/");
        for object in self
            .store
            .list(&prefix)
            .await
            .map_err(|e| store_error(&e))?
        {
            self.store
                .delete(&object.key)
                .await
                .map_err(|e| store_error(&e))?;
        }
        for (key, _) in self.kv.list("bk_chunk").await? {
            if key.starts_with(&prefix) {
                self.kv.del("bk_chunk", &key).await?;
            }
        }
        let key = format!("{tenant}/{id}");
        self.kv.del("bk_upload", &key).await?;
        self.kv.del("bk_backup", &key).await
    }

    /// Deletes every backup past its tenant's retention, keeping the newest of each tenant.
    ///
    /// # Errors
    /// `cp.internal` when a row or object cannot be read or removed.
    pub async fn purge_expired(&self) -> Result<usize, CpError> {
        let now = self.now();
        let mut by_tenant: BTreeMap<String, Vec<(String, BackupRec)>> = BTreeMap::new();
        for (key, text) in self.kv.list("bk_backup").await? {
            if let Some((tenant, id)) = key.split_once('/') {
                by_tenant
                    .entry(tenant.to_owned())
                    .or_default()
                    .push((id.to_owned(), from_json(&text)?));
            }
        }
        let mut removed = 0;
        for (tenant, mut backups) in by_tenant {
            let days = match self.licence(&tenant).await {
                Ok((_, days)) => days,
                Err(_) => DEFAULT_RETENTION_DAYS,
            };
            backups.sort_by_key(|(_, rec)| std::cmp::Reverse(rec.committed_at));
            for (id, rec) in backups.into_iter().skip(1) {
                if self.expired(&rec, days, now) {
                    self.remove(&tenant, &id).await?;
                    removed += 1;
                }
            }
        }
        Ok(removed)
    }

    fn expired(&self, rec: &BackupRec, days: u64, now: i64) -> bool {
        let keep = i64::try_from(days.saturating_mul(86_400)).unwrap_or(i64::MAX);
        now.saturating_sub(rec.committed_at) > keep
    }
}

fn reply(request: &RequestId, result: Result<(StatusCode, Value), CpError>) -> Response {
    match result {
        Ok((status, value)) => (status, Json(value)).into_response(),
        Err(error) => error.into_response_with(&request.0),
    }
}

// ---------------------------------------------------------------------------------------------
// Upload
// ---------------------------------------------------------------------------------------------

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BeginBody {
    backup_id: String,
    chunk_count: u32,
    cipher_bytes: u64,
}

async fn begin(
    State(st): State<Arc<CloudBackupState>>,
    Extension(host): Extension<HostIdentity>,
    Extension(request): Extension<RequestId>,
    body: Bytes,
) -> Response {
    let result = async {
        let req: BeginBody =
            serde_json::from_slice(&body).map_err(|_| CpError::invalid(&["body"]))?;
        checked_segment(&req.backup_id)?;
        if req.chunk_count == 0 || req.chunk_count > MAX_CHUNKS || req.cipher_bytes == 0 {
            return Err(CpError::invalid(&["chunk_count", "cipher_bytes"]));
        }
        let tenant = host.tenant.as_str();
        let (quota, _) = st.licence(tenant).await?;
        let key = format!("{tenant}/{}", req.backup_id);
        let rec = UploadRec {
            chunk_count: req.chunk_count,
            cipher_bytes: req.cipher_bytes,
            committed: false,
        };
        if let Some(text) = st.kv.get("bk_upload", &key).await? {
            let old: UploadRec = from_json(&text)?;
            if old.committed
                || old.chunk_count != rec.chunk_count
                || old.cipher_bytes != rec.cipher_bytes
            {
                return Err(CpError::with_reason(CpCode::Conflict, "backup.id_in_use"));
            }
        } else {
            let used = st.used(tenant).await?;
            if used.saturating_add(req.cipher_bytes) > quota {
                return Err(CpError::with_reason(
                    CpCode::LimitExceeded,
                    "backup.quota_exceeded",
                ));
            }
            st.kv.put("bk_upload", &key, &to_json(&rec)?).await?;
        }
        st.kv
            .put("bk_host", &format!("{tenant}/{}", host.host_id), "1")
            .await?;
        let received: Vec<u32> = st
            .chunks(tenant, &req.backup_id)
            .await?
            .into_iter()
            .map(|(index, _)| index)
            .collect();
        Ok((
            StatusCode::OK,
            json!({ "data": { "backup_id": req.backup_id, "received": received,
                              "quota_remaining": quota.saturating_sub(st.used(tenant).await?) }}),
        ))
    }
    .await;
    reply(&request, result)
}

#[derive(Deserialize, Default)]
struct ChunkQuery {
    generation: Option<u32>,
}

async fn put_chunk(
    State(st): State<Arc<CloudBackupState>>,
    Extension(host): Extension<HostIdentity>,
    Extension(request): Extension<RequestId>,
    Path((backup, index)): Path<(String, u32)>,
    Query(query): Query<ChunkQuery>,
    headers: axum::http::HeaderMap,
    body: Bytes,
) -> Response {
    let result = async {
        checked_segment(&backup)?;
        let tenant = host.tenant.as_str();
        let (quota, _) = st.licence(tenant).await?;
        let key = format!("{tenant}/{backup}");
        let upload: UploadRec = from_json(
            &st.kv
                .get("bk_upload", &key)
                .await?
                .ok_or_else(|| CpError::new(CpCode::NotFound))?,
        )?;
        if upload.committed {
            return Err(CpError::with_reason(CpCode::Conflict, "backup.committed"));
        }
        let claimed = headers
            .get("x-chunk-sha256")
            .and_then(|v| v.to_str().ok())
            .filter(|v| hex64(v))
            .ok_or_else(|| CpError::invalid(&["x-chunk-sha256"]))?
            .to_ascii_lowercase();
        let digest = sha256_hex(&body);
        if index >= upload.chunk_count || body.is_empty() || claimed != digest {
            return Err(CpError::invalid(&["chunk"]));
        }
        let generation = query.generation.unwrap_or(0);
        if generation > 9_999 {
            return Err(CpError::invalid(&["generation"]));
        }
        let object = BackupManifest::chunk_object(tenant, &backup, index, generation);
        let row_key = format!("{key}/{index:08}");
        let len = u64::try_from(body.len()).unwrap_or(u64::MAX);
        if let Some(text) = st.kv.get("bk_chunk", &row_key).await? {
            let old: ChunkRec = from_json(&text)?;
            if old.digest == digest && old.object == object {
                return Ok((StatusCode::OK, json!({ "data": { "stored": false } })));
            }
            return Err(CpError::with_reason(
                CpCode::Conflict,
                "backup.chunk_immutable",
            ));
        }
        if st.used(tenant).await?.saturating_add(len) > quota {
            return Err(CpError::with_reason(
                CpCode::LimitExceeded,
                "backup.quota_exceeded",
            ));
        }
        st.store
            .put(&object, body.to_vec())
            .await
            .map_err(|e| store_error(&e))?;
        let rec = ChunkRec {
            digest,
            len,
            object,
        };
        st.kv.put("bk_chunk", &row_key, &to_json(&rec)?).await?;
        Ok((StatusCode::OK, json!({ "data": { "stored": true } })))
    }
    .await;
    reply(&request, result)
}

async fn commit(
    State(st): State<Arc<CloudBackupState>>,
    Extension(host): Extension<HostIdentity>,
    Extension(request): Extension<RequestId>,
    Path(backup): Path<String>,
    body: Bytes,
) -> Response {
    let result = async {
        checked_segment(&backup)?;
        let tenant = host.tenant.as_str();
        st.licence(tenant).await?;
        let key = format!("{tenant}/{backup}");
        let mut upload: UploadRec = from_json(
            &st.kv
                .get("bk_upload", &key)
                .await?
                .ok_or_else(|| CpError::new(CpCode::NotFound))?,
        )?;
        let manifest: BackupManifest =
            serde_json::from_slice(&body).map_err(|_| CpError::invalid(&["manifest"]))?;
        let bad = || CpError::invalid(&["manifest"]);
        if manifest.tenant != tenant
            || manifest.backup_id != backup
            || manifest.version != 1
            || manifest.state != BackupState::Pending
            || manifest.key().is_empty()
        {
            return Err(bad());
        }
        if upload.committed {
            let rec = st.backup(tenant, &backup).await?;
            if rec.manifest_key == manifest.key() {
                return Ok((StatusCode::OK, json!({ "data": { "backup_id": backup } })));
            }
            return Err(CpError::with_reason(CpCode::Conflict, "backup.committed"));
        }
        let received: BTreeMap<u32, ChunkRec> =
            st.chunks(tenant, &backup).await?.into_iter().collect();
        let total: u64 = received.values().map(|c| c.len).sum();
        if received.len() != usize::try_from(upload.chunk_count).unwrap_or(usize::MAX)
            || total != upload.cipher_bytes
            || manifest.chunks.len() != received.len()
        {
            return Err(CpError::with_reason(CpCode::Conflict, "backup.incomplete"));
        }
        for entry in &manifest.chunks {
            let ok = received.get(&entry.index).is_some_and(|c| {
                c.digest == entry.sha256 && c.len == entry.cipher_len && c.object == entry.object
            });
            if !ok {
                return Err(bad());
            }
        }
        let manifest_key = manifest.key();
        st.store
            .put(&manifest_key, body.to_vec())
            .await
            .map_err(|e| store_error(&e))?;
        let rec = BackupRec {
            committed_at: st.now(),
            cipher_bytes: total,
            manifest_key,
        };
        st.kv.put("bk_backup", &key, &to_json(&rec)?).await?;
        upload.committed = true;
        st.kv.put("bk_upload", &key, &to_json(&upload)?).await?;
        Ok((StatusCode::OK, json!({ "data": { "backup_id": backup } })))
    }
    .await;
    reply(&request, result)
}

// ---------------------------------------------------------------------------------------------
// Reads, deletion, usage
// ---------------------------------------------------------------------------------------------

async fn list_backups(
    State(st): State<Arc<CloudBackupState>>,
    Extension(host): Extension<HostIdentity>,
    Extension(request): Extension<RequestId>,
) -> Response {
    let result = async {
        st.may_read(&host).await?;
        let prefix = format!("{}/", host.tenant);
        let mut rows = Vec::new();
        for (key, text) in st.kv.list("bk_backup").await? {
            if let Some(id) = key.strip_prefix(&prefix) {
                let rec: BackupRec = from_json(&text)?;
                rows.push(json!({ "backup_id": id, "committed_at": rec.committed_at,
                                  "cipher_bytes": rec.cipher_bytes }));
            }
        }
        Ok((StatusCode::OK, json!({ "data": rows })))
    }
    .await;
    reply(&request, result)
}

async fn get_manifest(
    State(st): State<Arc<CloudBackupState>>,
    Extension(host): Extension<HostIdentity>,
    Extension(request): Extension<RequestId>,
    Path(backup): Path<String>,
) -> Response {
    let result = async {
        checked_segment(&backup)?;
        st.may_read(&host).await?;
        let rec = st.backup(&host.tenant, &backup).await?;
        let bytes = st
            .store
            .get(&rec.manifest_key)
            .await
            .map_err(|e| store_error(&e))?;
        let manifest: Value = serde_json::from_slice(&bytes)
            .map_err(|_| CpError::with_reason(CpCode::Internal, "backup.store"))?;
        Ok((StatusCode::OK, json!({ "data": manifest })))
    }
    .await;
    reply(&request, result)
}

async fn get_chunk(
    State(st): State<Arc<CloudBackupState>>,
    Extension(host): Extension<HostIdentity>,
    Extension(request): Extension<RequestId>,
    Path((backup, index)): Path<(String, u32)>,
) -> Response {
    let result = async {
        checked_segment(&backup)?;
        st.may_read(&host).await?;
        let row = st
            .kv
            .get("bk_chunk", &format!("{}/{backup}/{index:08}", host.tenant))
            .await?
            .ok_or_else(|| CpError::new(CpCode::NotFound))?;
        st.backup(&host.tenant, &backup).await?;
        let chunk: ChunkRec = from_json(&row)?;
        let bytes = st
            .store
            .get(&chunk.object)
            .await
            .map_err(|e| store_error(&e))?;
        Ok::<_, CpError>((chunk.digest, bytes))
    }
    .await;
    match result {
        Ok((digest, bytes)) => (
            StatusCode::OK,
            [
                (header::CONTENT_TYPE, "application/octet-stream".to_owned()),
                (header::HeaderName::from_static("x-chunk-sha256"), digest),
            ],
            bytes,
        )
            .into_response(),
        Err(error) => error.into_response_with(&request.0),
    }
}

async fn host_delete(
    State(st): State<Arc<CloudBackupState>>,
    Extension(host): Extension<HostIdentity>,
    Extension(request): Extension<RequestId>,
    Path(backup): Path<String>,
) -> Response {
    let result = async {
        checked_segment(&backup)?;
        let (_, days) = st.licence(&host.tenant).await?;
        let rec = st.backup(&host.tenant, &backup).await?;
        if !st.expired(&rec, days, st.now()) {
            return Err(CpError::with_reason(
                CpCode::Forbidden,
                "backup.retention_active",
            ));
        }
        st.remove(&host.tenant, &backup).await?;
        Ok((StatusCode::OK, json!({ "data": { "deleted": backup } })))
    }
    .await;
    reply(&request, result)
}

async fn usage(
    State(st): State<Arc<CloudBackupState>>,
    Extension(host): Extension<HostIdentity>,
    Extension(request): Extension<RequestId>,
) -> Response {
    let result = async {
        let (quota, days) = st.licence(&host.tenant).await?;
        Ok((
            StatusCode::OK,
            json!({ "data": { "used_bytes": st.used(&host.tenant).await?,
                              "quota_bytes": quota, "retention_days": days }}),
        ))
    }
    .await;
    reply(&request, result)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DeleteBody {
    reason: String,
}

async fn operator_delete(
    State(st): State<Arc<CloudBackupState>>,
    Extension(call): Extension<OperatorCall>,
    Path((tenant, backup)): Path<(String, String)>,
    body: Bytes,
) -> Response {
    let result = async {
        call.scope.require(&tenant)?;
        checked_segment(&backup)?;
        let req: DeleteBody =
            serde_json::from_slice(&body).map_err(|_| CpError::invalid(&["body"]))?;
        if req.reason.is_empty() || req.reason.len() > 500 {
            return Err(CpError::invalid(&["reason"]));
        }
        st.backup(&tenant, &backup).await?;
        let record = json!({ "at": st.now(), "by": call.principal, "reason": req.reason });
        st.kv
            .put(
                "bk_deleted",
                &format!("{tenant}/{backup}"),
                &record.to_string(),
            )
            .await?;
        st.remove(&tenant, &backup).await?;
        Ok((StatusCode::OK, json!({ "data": { "deleted": backup } })))
    }
    .await;
    finish(&call, result)
}

// ---------------------------------------------------------------------------------------------
// Recovery
// ---------------------------------------------------------------------------------------------

async fn issue_code(
    State(st): State<Arc<CloudBackupState>>,
    Extension(host): Extension<HostIdentity>,
    Extension(request): Extension<RequestId>,
) -> Response {
    let result = async {
        st.licence(&host.tenant).await?;
        let owner = format!("{}/{}", host.tenant, host.host_id);
        if st.kv.get("bk_host", &owner).await?.is_none() {
            return Err(CpError::with_reason(
                CpCode::Forbidden,
                "backup.not_a_backup_host",
            ));
        }
        if let Some(text) = st.kv.get("bk_code", &host.tenant).await? {
            let rec: CodeRec = from_json(&text)?;
            if st.kv.get("bk_code_used", &rec.digest).await?.is_none() {
                return Err(CpError::with_reason(CpCode::Conflict, "backup.code_exists"));
            }
        }
        let code = format!("spot-rc-{}", random_hex(16).map_err(|e| store_error(&e))?);
        let rec = CodeRec {
            digest: sha256_hex(code.as_bytes()),
            issued_at: st.now(),
        };
        st.kv.put("bk_code", &host.tenant, &to_json(&rec)?).await?;
        Ok((
            StatusCode::CREATED,
            json!({ "data": { "recovery_code": code, "shown_once": true } }),
        ))
    }
    .await;
    reply(&request, result)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RedeemBody {
    code: String,
}

async fn redeem(
    State(st): State<Arc<CloudBackupState>>,
    Extension(host): Extension<HostIdentity>,
    Extension(request): Extension<RequestId>,
    body: Bytes,
) -> Response {
    let result = async {
        let req: RedeemBody =
            serde_json::from_slice(&body).map_err(|_| CpError::invalid(&["body"]))?;
        st.licence(&host.tenant).await?;
        let now = st.now();
        let mut rate: RateRec = match st.kv.get("bk_rate", &host.tenant).await? {
            Some(text) => from_json(&text)?,
            None => RateRec::default(),
        };
        if now.saturating_sub(rate.start) > RECOVERY_WINDOW_SECS {
            rate = RateRec {
                start: now,
                fails: 0,
            };
        }
        if rate.fails >= RECOVERY_FAILS {
            return Err(CpError::new(CpCode::RateLimited));
        }
        let stored: Option<CodeRec> = match st.kv.get("bk_code", &host.tenant).await? {
            Some(text) => Some(from_json(&text)?),
            None => None,
        };
        let digest = sha256_hex(req.code.as_bytes());
        let matches = stored.as_ref().is_some_and(|c| c.digest == digest);
        // The use is recorded atomically: two redemptions of one code cannot both succeed.
        let spent = matches && st.kv.put_new("bk_code_used", &digest, "1").await?;
        if !spent {
            rate.fails += 1;
            st.kv.put("bk_rate", &host.tenant, &to_json(&rate)?).await?;
            return Err(CpError::with_reason(
                CpCode::Forbidden,
                "backup.code_invalid",
            ));
        }
        let until = now.saturating_add(GRANT_SECS);
        st.kv
            .put(
                "bk_grant",
                &format!("{}/{}", host.tenant, host.host_id),
                &until.to_string(),
            )
            .await?;
        Ok((
            StatusCode::OK,
            json!({ "data": { "grant_expires_at": until } }),
        ))
    }
    .await;
    reply(&request, result)
}

/// The routes of the module: operator routes and host routes.
pub fn routes(state: Arc<CloudBackupState>) -> (Router, Router) {
    let operator = Router::new()
        .route(
            "/cp/v1/tenants/{id}/backups/{backup}/delete",
            post(operator_delete),
        )
        .with_state(Arc::clone(&state));
    let host = Router::new()
        .route("/cp/v1/host/backup/uploads", post(begin))
        .route(
            "/cp/v1/host/backup/uploads/{backup}/chunks/{index}",
            put(put_chunk).layer(DefaultBodyLimit::max(MAX_CHUNK_BODY)),
        )
        .route(
            "/cp/v1/host/backup/uploads/{backup}/commit",
            post(commit).layer(DefaultBodyLimit::max(MAX_MANIFEST_BODY)),
        )
        .route("/cp/v1/host/backup/backups", get(list_backups))
        .route("/cp/v1/host/backup/backups/{backup}", delete(host_delete))
        .route(
            "/cp/v1/host/backup/backups/{backup}/manifest",
            get(get_manifest),
        )
        .route(
            "/cp/v1/host/backup/backups/{backup}/chunks/{index}",
            get(get_chunk),
        )
        .route("/cp/v1/host/backup/usage", get(usage))
        .route("/cp/v1/host/backup/recovery-code", post(issue_code))
        .route("/cp/v1/host/backup/recovery/redeem", post(redeem))
        .with_state(state);
    (operator, host)
}

/// Mounts the module over a prepared state and starts the retention timer.
pub fn register_with(ctx: &mut CpCtx, state: Arc<CloudBackupState>) {
    let (operator, host) = routes(Arc::clone(&state));
    ctx.operator_routes(operator);
    ctx.host_routes(host);
    ctx.add_migration(DDL);
    ctx.add_task(Box::new(move |mut stop| -> spot_mgmt_api::ports::BoxFuture<'static, ()> {
        Box::pin(async move {
            loop {
                let stopped = tokio::select! {
                    changed = stop.changed() => changed.is_err() || *stop.borrow(),
                    () = tokio::time::sleep(std::time::Duration::from_secs(PURGE_EVERY_SECS)) => false,
                };
                if stopped {
                    break;
                }
                let _ = state.purge_expired().await;
            }
        })
    }));
}

/// Registers the module over the control database and the filesystem store named by
/// `SPOT_CP_BACKUP_DIR`; without it the module is not mounted and readiness reports it as not
/// ready. An S3-compatible backend is mounted by a caller through [`register_with`].
///
/// # Errors
/// `cp.internal` when there is no control database.
pub fn register(ctx: &mut CpCtx) -> Result<(), CpError> {
    let Ok(dir) = std::env::var("SPOT_CP_BACKUP_DIR") else {
        ctx.add_readiness(Arc::new(
            || -> spot_mgmt_api::ports::BoxFuture<'static, bool> { Box::pin(async { false }) },
        ));
        return Ok(());
    };
    let pool = ctx
        .pool
        .clone()
        .ok_or_else(|| CpError::with_reason(CpCode::Internal, "backup.no_control_database"))?;
    let store = spot_cloud_backup::FileSystemStore::new(std::path::PathBuf::from(dir));
    let kv: Arc<dyn Kv> = Arc::new(PgKv::new(pool));
    register_with(
        ctx,
        CloudBackupState::new(kv, Arc::new(store), Arc::clone(&ctx.clock)),
    );
    Ok(())
}
