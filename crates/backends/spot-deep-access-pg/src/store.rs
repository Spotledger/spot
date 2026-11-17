//! [`PgDeepAccessStore`]: the async store the hosting service uses, for one tenant.

use deadpool_postgres::{Object, Pool};
use tokio_postgres::Row;

use crate::error::{StoreError, failed, pool_failed};

/// The longest id, name or key text accepted (the schema checks ids at 200 bytes).
const MAX_TEXT: usize = 200;

/// A request's lifecycle. A request is never deleted: the customer sees past requests too.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RequestStatus {
    /// Waiting for the customer.
    Pending,
    /// Enough approvals recorded.
    Approved,
    /// The customer refused it.
    Rejected,
    /// A credential was issued from it.
    Issued,
    /// The customer revoked it (or its session).
    Revoked,
    /// Its window passed.
    Expired,
}

impl RequestStatus {
    /// The stored text.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Approved => "approved",
            Self::Rejected => "rejected",
            Self::Issued => "issued",
            Self::Revoked => "revoked",
            Self::Expired => "expired",
        }
    }

    /// Parses the stored text.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        Some(match text {
            "pending" => Self::Pending,
            "approved" => Self::Approved,
            "rejected" => Self::Rejected,
            "issued" => Self::Issued,
            "revoked" => Self::Revoked,
            "expired" => Self::Expired,
            _ => return None,
        })
    }
}

/// One stored request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RequestRow {
    /// The request id.
    pub id: String,
    /// The request as the operator sent it (JSON text).
    pub request: String,
    /// Its hash (lower-case hex), the value a consent must name.
    pub request_hash: String,
    /// Where it is in its lifecycle.
    pub status: RequestStatus,
    /// When it was made (Unix seconds).
    pub requested_at: i64,
    /// When its window ends.
    pub expires_at: i64,
}

/// One recorded approval.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConsentRow {
    /// The request approved.
    pub request_id: String,
    /// The approving identity.
    pub approver: String,
    /// The id of the key that signed.
    pub approver_key: String,
    /// The signed record (JSON text).
    pub record: String,
    /// The signature bytes.
    pub signature: Vec<u8>,
    /// The receipt hash (lower-case hex).
    pub receipt_hash: String,
    /// When it was recorded.
    pub recorded_at: i64,
}

/// What was issued. The secret is never here, only its digest.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CredentialRow {
    /// The session id.
    pub session_id: [u8; 16],
    /// The request it came from.
    pub request_id: String,
    /// The request hash.
    pub request_hash: String,
    /// The receipt hash.
    pub receipt_hash: String,
    /// The scope, as text the customer can read.
    pub scope: String,
    /// Window start.
    pub starts_at: i64,
    /// Window end (exclusive).
    pub expires_at: i64,
    /// When it was issued.
    pub issued_at: i64,
}

/// An enrolled customer signing key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyRow {
    /// The key id.
    pub key_id: String,
    /// The identity it belongs to.
    pub identity: String,
    /// The public key.
    pub public_key: [u8; 32],
    /// Whether the identity is an owner of the tenant.
    pub is_owner: bool,
    /// Who enrolled it (a step-up administrator).
    pub enrolled_by: String,
    /// When.
    pub enrolled_at: i64,
}

/// The deep access store of one tenant.
#[derive(Clone)]
pub struct PgDeepAccessStore {
    pool: Pool,
    tenant: String,
}

impl std::fmt::Debug for PgDeepAccessStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PgDeepAccessStore")
            .field("tenant", &self.tenant)
            .finish_non_exhaustive()
    }
}

fn text(value: &str) -> Result<(), StoreError> {
    if value.is_empty() || value.len() > MAX_TEXT || value.contains('\0') {
        return Err(StoreError::Invalid(
            "a text is empty, over 200 bytes or holds a NUL",
        ));
    }
    Ok(())
}

fn request_row(row: &Row) -> Result<RequestRow, StoreError> {
    let status: String = row.get("status");
    Ok(RequestRow {
        id: row.get("id"),
        request: row.get("request"),
        request_hash: row.get("request_hash"),
        status: RequestStatus::parse(&status).ok_or(StoreError::Failed)?,
        requested_at: row.get("requested_at"),
        expires_at: row.get("expires_at"),
    })
}

fn array<const N: usize>(bytes: Vec<u8>) -> Result<[u8; N], StoreError> {
    bytes.try_into().map_err(|_| StoreError::Failed)
}

impl PgDeepAccessStore {
    /// A store for `tenant` on the tenant's own pool.
    ///
    /// # Errors
    /// [`StoreError::Invalid`] for an empty or over-long tenant.
    pub fn new(pool: Pool, tenant: &str) -> Result<Self, StoreError> {
        text(tenant)?;
        Ok(Self {
            pool,
            tenant: tenant.to_owned(),
        })
    }

    /// The tenant every statement is scoped to.
    #[must_use]
    pub fn tenant(&self) -> &str {
        &self.tenant
    }

    /// Creates the tables if missing.
    ///
    /// # Errors
    /// `deep_access_pg.migration_failed`.
    pub async fn prepare(&self) -> Result<(), StoreError> {
        crate::schema::prepare(&self.pool).await
    }

    async fn conn(&self) -> Result<Object, StoreError> {
        self.pool.get().await.map_err(|e| pool_failed(&e))
    }

    /// Stores a new request as `pending`; `false` when the id is already taken (the first one
    /// stays; an operator cannot overwrite a request).
    ///
    /// # Errors
    /// [`StoreError`] when the database cannot answer.
    pub async fn put_request(&self, row: &RequestRow, now: i64) -> Result<bool, StoreError> {
        text(&row.id)?;
        let n = self
            .conn()
            .await?
            .execute(
                "INSERT INTO spot_deep_access_requests \
                 (tenant, id, request, request_hash, status, requested_at, expires_at, updated_at) \
                 VALUES ($1,$2,$3,$4,'pending',$5,$6,$7) ON CONFLICT DO NOTHING",
                &[
                    &self.tenant,
                    &row.id,
                    &row.request,
                    &row.request_hash,
                    &row.requested_at,
                    &row.expires_at,
                    &now,
                ],
            )
            .await
            .map_err(|e| failed(&e))?;
        Ok(n == 1)
    }

    /// One request of this tenant; another tenant's id answers `None`.
    ///
    /// # Errors
    /// [`StoreError`] when the database cannot answer.
    pub async fn get_request(&self, id: &str) -> Result<Option<RequestRow>, StoreError> {
        let row = self
            .conn()
            .await?
            .query_opt(
                "SELECT id, request, request_hash, status, requested_at, expires_at \
                 FROM spot_deep_access_requests WHERE tenant = $1 AND id = $2",
                &[&self.tenant, &id],
            )
            .await
            .map_err(|e| failed(&e))?;
        row.as_ref().map(request_row).transpose()
    }

    /// Requests in id order after `after`, at most `limit` (1 to 100), optionally of one status.
    ///
    /// # Errors
    /// [`StoreError`] when the database cannot answer.
    pub async fn list_requests(
        &self,
        status: Option<RequestStatus>,
        after: Option<&str>,
        limit: u32,
    ) -> Result<Vec<RequestRow>, StoreError> {
        let limit = i64::from(limit.clamp(1, 100));
        let status = status.map(RequestStatus::as_str);
        let after = after.unwrap_or("");
        let rows = self
            .conn()
            .await?
            .query(
                "SELECT id, request, request_hash, status, requested_at, expires_at \
                 FROM spot_deep_access_requests \
                 WHERE tenant = $1 AND id > $2 AND ($3::text IS NULL OR status = $3) \
                 ORDER BY id LIMIT $4",
                &[&self.tenant, &after, &status, &limit],
            )
            .await
            .map_err(|e| failed(&e))?;
        rows.iter().map(request_row).collect()
    }

    /// Moves a request to `to` when it is currently one of `from`; `false` when it is not (so a
    /// revoked request cannot be approved, an expired one cannot be issued).
    ///
    /// # Errors
    /// [`StoreError`] when the database cannot answer.
    pub async fn transition(
        &self,
        id: &str,
        from: &[RequestStatus],
        to: RequestStatus,
        now: i64,
    ) -> Result<bool, StoreError> {
        let from: Vec<&str> = from.iter().map(|s| s.as_str()).collect();
        let n = self
            .conn()
            .await?
            .execute(
                "UPDATE spot_deep_access_requests SET status = $3, updated_at = $5 \
                 WHERE tenant = $1 AND id = $2 AND status = ANY($4)",
                &[&self.tenant, &id, &to.as_str(), &from, &now],
            )
            .await
            .map_err(|e| failed(&e))?;
        Ok(n == 1)
    }

    /// Records one approval; `false` when this approver already approved this request.
    ///
    /// # Errors
    /// [`StoreError`] when the database cannot answer.
    pub async fn record_consent(&self, row: &ConsentRow) -> Result<bool, StoreError> {
        text(&row.request_id)?;
        text(&row.approver)?;
        let n = self
            .conn()
            .await?
            .execute(
                "INSERT INTO spot_deep_access_consents \
                 (tenant, request_id, approver, approver_key, record, signature, receipt_hash, recorded_at) \
                 VALUES ($1,$2,$3,$4,$5,$6,$7,$8) ON CONFLICT DO NOTHING",
                &[
                    &self.tenant,
                    &row.request_id,
                    &row.approver,
                    &row.approver_key,
                    &row.record,
                    &row.signature,
                    &row.receipt_hash,
                    &row.recorded_at,
                ],
            )
            .await
            .map_err(|e| failed(&e))?;
        Ok(n == 1)
    }

    /// The approvals recorded for a request, oldest first.
    ///
    /// # Errors
    /// [`StoreError`] when the database cannot answer.
    pub async fn consents(&self, request_id: &str) -> Result<Vec<ConsentRow>, StoreError> {
        let rows = self
            .conn()
            .await?
            .query(
                "SELECT request_id, approver, approver_key, record, signature, receipt_hash, recorded_at \
                 FROM spot_deep_access_consents WHERE tenant = $1 AND request_id = $2 \
                 ORDER BY recorded_at, approver",
                &[&self.tenant, &request_id],
            )
            .await
            .map_err(|e| failed(&e))?;
        Ok(rows
            .iter()
            .map(|r| ConsentRow {
                request_id: r.get("request_id"),
                approver: r.get("approver"),
                approver_key: r.get("approver_key"),
                record: r.get("record"),
                signature: r.get("signature"),
                receipt_hash: r.get("receipt_hash"),
                recorded_at: r.get("recorded_at"),
            })
            .collect())
    }

    /// Consumes a receipt nonce: `true` once, `false` for every later use, on every node.
    ///
    /// # Errors
    /// [`StoreError`] when the database cannot answer; the caller refuses (fail closed).
    pub async fn consume_nonce(&self, key_id: &str, nonce: [u8; 16]) -> Result<bool, StoreError> {
        let nonce = nonce.to_vec();
        let n = self
            .conn()
            .await?
            .execute(
                crate::bridge::CONSUME_NONCE,
                &[&self.tenant, &key_id, &nonce],
            )
            .await
            .map_err(|e| failed(&e))?;
        Ok(n == 1)
    }

    /// Records a revocation; the first one stays.
    ///
    /// # Errors
    /// [`StoreError`] when the database cannot answer.
    pub async fn revoke(
        &self,
        session_id: [u8; 16],
        by: &str,
        reason: &str,
        at: i64,
    ) -> Result<(), StoreError> {
        let id = session_id.to_vec();
        self.conn()
            .await?
            .execute(
                crate::bridge::INSERT_REVOCATION,
                &[&self.tenant, &id, &by, &reason, &at],
            )
            .await
            .map_err(|e| failed(&e))?;
        Ok(())
    }

    /// The revocation of a session, if any: `(by, reason, at)`.
    ///
    /// # Errors
    /// [`StoreError`] when the database cannot answer.
    pub async fn revocation(
        &self,
        session_id: [u8; 16],
    ) -> Result<Option<(String, String, i64)>, StoreError> {
        let id = session_id.to_vec();
        let row = self
            .conn()
            .await?
            .query_opt(crate::bridge::LOOKUP_REVOCATION, &[&self.tenant, &id])
            .await
            .map_err(|e| failed(&e))?;
        Ok(row.map(|r| (r.get(0), r.get(1), r.get(2))))
    }

    /// Records an issued credential: its scope and window and the digest of its secret.
    ///
    /// # Errors
    /// [`StoreError`] when the database cannot answer.
    pub async fn record_credential(
        &self,
        row: &CredentialRow,
        secret_digest: [u8; 32],
    ) -> Result<(), StoreError> {
        let id = row.session_id.to_vec();
        let digest = secret_digest.to_vec();
        self.conn()
            .await?
            .execute(
                "INSERT INTO spot_deep_access_credentials \
                 (tenant, session_id, request_id, request_hash, receipt_hash, scope, starts_at, expires_at, secret_digest, issued_at) \
                 VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10) ON CONFLICT DO NOTHING",
                &[
                    &self.tenant,
                    &id,
                    &row.request_id,
                    &row.request_hash,
                    &row.receipt_hash,
                    &row.scope,
                    &row.starts_at,
                    &row.expires_at,
                    &digest,
                    &row.issued_at,
                ],
            )
            .await
            .map_err(|e| failed(&e))?;
        Ok(())
    }

    /// The credentials of this tenant, newest first, with whether each was revoked.
    ///
    /// # Errors
    /// [`StoreError`] when the database cannot answer.
    pub async fn credentials(&self, limit: u32) -> Result<Vec<(CredentialRow, bool)>, StoreError> {
        let limit = i64::from(limit.clamp(1, 100));
        let rows = self
            .conn()
            .await?
            .query(
                "SELECT c.session_id, c.request_id, c.request_hash, c.receipt_hash, c.scope, \
                        c.starts_at, c.expires_at, c.issued_at, \
                        (r.session_id IS NOT NULL) AS revoked \
                 FROM spot_deep_access_credentials c \
                 LEFT JOIN spot_deep_access_revocations r \
                   ON r.tenant = c.tenant AND r.session_id = c.session_id \
                 WHERE c.tenant = $1 ORDER BY c.issued_at DESC, c.session_id LIMIT $2",
                &[&self.tenant, &limit],
            )
            .await
            .map_err(|e| failed(&e))?;
        rows.iter()
            .map(|r| {
                Ok((
                    CredentialRow {
                        session_id: array(r.get("session_id"))?,
                        request_id: r.get("request_id"),
                        request_hash: r.get("request_hash"),
                        receipt_hash: r.get("receipt_hash"),
                        scope: r.get("scope"),
                        starts_at: r.get("starts_at"),
                        expires_at: r.get("expires_at"),
                        issued_at: r.get("issued_at"),
                    },
                    r.get("revoked"),
                ))
            })
            .collect()
    }

    /// Enrols a customer signing key; `false` when the key id is already enrolled.
    ///
    /// # Errors
    /// [`StoreError`] when the database cannot answer.
    pub async fn enrol_key(&self, row: &KeyRow) -> Result<bool, StoreError> {
        text(&row.key_id)?;
        text(&row.identity)?;
        let key = row.public_key.to_vec();
        let n = self
            .conn()
            .await?
            .execute(
                "INSERT INTO spot_deep_access_keys \
                 (tenant, key_id, identity, public_key, is_owner, enrolled_by, enrolled_at) \
                 VALUES ($1,$2,$3,$4,$5,$6,$7) ON CONFLICT DO NOTHING",
                &[
                    &self.tenant,
                    &row.key_id,
                    &row.identity,
                    &key,
                    &row.is_owner,
                    &row.enrolled_by,
                    &row.enrolled_at,
                ],
            )
            .await
            .map_err(|e| failed(&e))?;
        Ok(n == 1)
    }

    /// Every enrolled key of the tenant.
    ///
    /// # Errors
    /// [`StoreError`] when the database cannot answer.
    pub async fn keys(&self) -> Result<Vec<KeyRow>, StoreError> {
        let rows = self
            .conn()
            .await?
            .query(
                "SELECT key_id, identity, public_key, is_owner, enrolled_by, enrolled_at \
                 FROM spot_deep_access_keys WHERE tenant = $1 ORDER BY key_id",
                &[&self.tenant],
            )
            .await
            .map_err(|e| failed(&e))?;
        rows.iter()
            .map(|r| {
                Ok(KeyRow {
                    key_id: r.get("key_id"),
                    identity: r.get("identity"),
                    public_key: array(r.get("public_key"))?,
                    is_owner: r.get("is_owner"),
                    enrolled_by: r.get("enrolled_by"),
                    enrolled_at: r.get("enrolled_at"),
                })
            })
            .collect()
    }

    /// The sweep: pending and approved requests whose window ended become `expired`, and nonces
    /// older than `nonce_keep_secs` are removed (a receipt's window is hours, so a nonce kept for
    /// months cannot be replayed). Credentials and revocations are history and stay. Returns the
    /// number of requests expired and nonces removed, at most `limit` of each.
    ///
    /// # Errors
    /// [`StoreError`] when the database cannot answer.
    pub async fn sweep(
        &self,
        now: i64,
        nonce_keep_secs: i64,
        limit: u32,
    ) -> Result<(u64, u64), StoreError> {
        let limit = i64::from(limit.clamp(1, 10_000));
        let conn = self.conn().await?;
        let expired = conn
            .execute(
                "UPDATE spot_deep_access_requests SET status = 'expired', updated_at = $2 \
                 WHERE (tenant, id) IN ( \
                   SELECT tenant, id FROM spot_deep_access_requests \
                   WHERE tenant = $1 AND status IN ('pending','approved') AND expires_at <= $2 \
                   LIMIT $3)",
                &[&self.tenant, &now, &limit],
            )
            .await
            .map_err(|e| failed(&e))?;
        let cutoff = now.saturating_sub(nonce_keep_secs);
        let removed = conn
            .execute(
                "DELETE FROM spot_deep_access_nonces WHERE (tenant, key_id, nonce) IN ( \
                   SELECT tenant, key_id, nonce FROM spot_deep_access_nonces \
                   WHERE tenant = $1 AND used_at <= $2 LIMIT $3)",
                &[&self.tenant, &cutoff, &limit],
            )
            .await
            .map_err(|e| failed(&e))?;
        Ok((expired, removed))
    }
}
