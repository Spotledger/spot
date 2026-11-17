//! The DDL. Every statement is idempotent; `prepare` serialises nodes with an advisory lock.

use deadpool_postgres::Pool;

use crate::error::{StoreError, describe, pool_failed};

const PREPARE_LOCK: i64 = 0x4445_4150_0001;

/// The idempotent DDL.
pub const DDL: &str = r#"
CREATE TABLE IF NOT EXISTS spot_deep_access_requests (
    tenant       text   NOT NULL,
    id           text   NOT NULL,
    request      text   NOT NULL,
    request_hash text   NOT NULL,
    status       text   NOT NULL,
    requested_at bigint NOT NULL,
    expires_at   bigint NOT NULL,
    updated_at   bigint NOT NULL,
    PRIMARY KEY (tenant, id),
    CONSTRAINT spot_deep_access_requests_status CHECK (
        status IN ('pending','approved','rejected','issued','revoked','expired')),
    CONSTRAINT spot_deep_access_requests_size CHECK (
        octet_length(id) BETWEEN 1 AND 200 AND octet_length(request) <= 65536)
);
CREATE INDEX IF NOT EXISTS spot_deep_access_requests_status_idx
    ON spot_deep_access_requests (tenant, status, requested_at);

CREATE TABLE IF NOT EXISTS spot_deep_access_consents (
    tenant       text   NOT NULL,
    request_id   text   NOT NULL,
    approver     text   NOT NULL,
    approver_key text   NOT NULL,
    record       text   NOT NULL,
    signature    bytea  NOT NULL,
    receipt_hash text   NOT NULL,
    recorded_at  bigint NOT NULL,
    PRIMARY KEY (tenant, request_id, approver),
    CONSTRAINT spot_deep_access_consents_size CHECK (
        octet_length(record) <= 8192 AND octet_length(signature) <= 1024)
);

CREATE TABLE IF NOT EXISTS spot_deep_access_nonces (
    tenant   text  NOT NULL,
    key_id   text  NOT NULL,
    nonce    bytea NOT NULL,
    used_at  bigint NOT NULL DEFAULT 0,
    PRIMARY KEY (tenant, key_id, nonce),
    CONSTRAINT spot_deep_access_nonces_len CHECK (octet_length(nonce) = 16)
);

CREATE TABLE IF NOT EXISTS spot_deep_access_revocations (
    tenant     text   NOT NULL,
    session_id bytea  NOT NULL,
    revoked_by text   NOT NULL,
    reason     text   NOT NULL,
    revoked_at bigint NOT NULL,
    PRIMARY KEY (tenant, session_id),
    CONSTRAINT spot_deep_access_revocations_len CHECK (octet_length(session_id) = 16)
);

CREATE TABLE IF NOT EXISTS spot_deep_access_credentials (
    tenant        text   NOT NULL,
    session_id    bytea  NOT NULL,
    request_id    text   NOT NULL,
    request_hash  text   NOT NULL,
    receipt_hash  text   NOT NULL,
    scope         text   NOT NULL,
    starts_at     bigint NOT NULL,
    expires_at    bigint NOT NULL,
    secret_digest bytea  NOT NULL,
    issued_at     bigint NOT NULL,
    PRIMARY KEY (tenant, session_id),
    CONSTRAINT spot_deep_access_credentials_len CHECK (
        octet_length(session_id) = 16 AND octet_length(secret_digest) = 32)
);
CREATE INDEX IF NOT EXISTS spot_deep_access_credentials_expiry_idx
    ON spot_deep_access_credentials (tenant, expires_at);

CREATE TABLE IF NOT EXISTS spot_deep_access_keys (
    tenant      text   NOT NULL,
    key_id      text   NOT NULL,
    identity    text   NOT NULL,
    public_key  bytea  NOT NULL,
    is_owner    boolean NOT NULL,
    enrolled_by text   NOT NULL,
    enrolled_at bigint NOT NULL,
    PRIMARY KEY (tenant, key_id),
    CONSTRAINT spot_deep_access_keys_len CHECK (octet_length(public_key) = 32)
);
"#;

/// Creates the tables and indexes if missing; idempotent and safe from several nodes at once.
///
/// # Errors
/// [`StoreError::Migration`] with a SQLSTATE, never a row.
pub async fn prepare(pool: &Pool) -> Result<(), StoreError> {
    let mut conn = pool
        .get()
        .await
        .map_err(|e| StoreError::Migration(pool_failed(&e).to_string()))?;
    let tx = conn
        .transaction()
        .await
        .map_err(|e| StoreError::Migration(describe(&e)))?;
    tx.execute("SELECT pg_advisory_xact_lock($1)", &[&PREPARE_LOCK])
        .await
        .map_err(|e| StoreError::Migration(describe(&e)))?;
    tx.batch_execute(DDL)
        .await
        .map_err(|e| StoreError::Migration(describe(&e)))?;
    tx.commit()
        .await
        .map_err(|e| StoreError::Migration(describe(&e)))
}
