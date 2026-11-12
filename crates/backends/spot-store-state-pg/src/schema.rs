//! The control-schema tables, created idempotently.
//!
//! | Table | Holds |
//! | --- | --- |
//! | `spot_control.spot_store_state_meta` | one row: the revocation list generation counter |
//! | `spot_control.spot_store_state_key` | publisher keys by SHA-256 (unique), label, issue and revoke times |
//! | `spot_control.spot_store_state_review` | review records with a bounded summary |
//! | `spot_control.spot_store_state_listing` | the listing state of `(app, version)`; only `listed` lists |
//! | `spot_control.spot_store_state_idempotency` | `(publisher, key)` with the request fingerprint and the review |
//! | `spot_control.spot_store_state_revoked_key` | revoked signing keys |
//! | `spot_control.spot_store_state_revoked_bundle` | revoked bundle digests |
//! | `spot_control.spot_store_state_issued_list` | one row: the last issued revocation list, as text |
//! | `spot_control.spot_store_state_audit` | the append-only audit log |
//!
//! Plain PostgreSQL, no extension. Every bounded field also has a check constraint, so a writer
//! that skipped the port's validation is still refused by the database.

use tokio_postgres::Client;

/// The control schema, shared with `spot-tenancy-pg` and `spot-registry-pg`.
pub const CONTROL_SCHEMA: &str = "spot_control";

const SCHEMA_LOCK: i64 = 0x5354_4154_4501;

/// The idempotent DDL.
pub const DDL: &str = "\
CREATE SCHEMA IF NOT EXISTS spot_control; \
CREATE TABLE IF NOT EXISTS spot_control.spot_store_state_meta (\
    id smallint PRIMARY KEY CHECK (id = 1), \
    generation bigint NOT NULL CHECK (generation >= 0), \
    updated_at bigint NOT NULL\
); \
INSERT INTO spot_control.spot_store_state_meta (id, generation, updated_at) VALUES (1, 0, 0) \
    ON CONFLICT (id) DO NOTHING; \
CREATE TABLE IF NOT EXISTS spot_control.spot_store_state_key (\
    key_id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY, \
    publisher_id text NOT NULL CHECK (octet_length(publisher_id) BETWEEN 1 AND 128), \
    key_hash bytea NOT NULL CONSTRAINT spot_store_state_key_hash_key UNIQUE \
        CHECK (octet_length(key_hash) = 32), \
    label text NOT NULL CHECK (octet_length(label) BETWEEN 1 AND 128), \
    issued_at bigint NOT NULL, \
    revoked_at bigint\
); \
CREATE INDEX IF NOT EXISTS spot_store_state_key_publisher_idx \
    ON spot_control.spot_store_state_key (publisher_id, key_id); \
CREATE TABLE IF NOT EXISTS spot_control.spot_store_state_review (\
    review_id text PRIMARY KEY CHECK (octet_length(review_id) BETWEEN 1 AND 128), \
    publisher_id text NOT NULL CHECK (octet_length(publisher_id) BETWEEN 1 AND 128), \
    app text NOT NULL, \
    version text NOT NULL, \
    digest text NOT NULL, \
    status text NOT NULL CHECK (status IN ('queued', 'running', 'done', 'failed')), \
    verdict text CHECK (octet_length(verdict) BETWEEN 1 AND 128), \
    summary text CHECK (octet_length(summary) <= 65536), \
    error_code text CHECK (octet_length(error_code) BETWEEN 1 AND 128), \
    created_at bigint NOT NULL, \
    updated_at bigint NOT NULL\
); \
CREATE INDEX IF NOT EXISTS spot_store_state_review_version_idx \
    ON spot_control.spot_store_state_review (app, version, created_at, review_id); \
CREATE TABLE IF NOT EXISTS spot_control.spot_store_state_listing (\
    app text NOT NULL, \
    version text NOT NULL, \
    state text NOT NULL \
        CHECK (state IN ('pending_review', 'listed', 'rejected', 'withdrawn')), \
    set_by text NOT NULL CHECK (octet_length(set_by) BETWEEN 1 AND 128), \
    set_at bigint NOT NULL, \
    PRIMARY KEY (app, version)\
); \
CREATE TABLE IF NOT EXISTS spot_control.spot_store_state_idempotency (\
    publisher_id text NOT NULL CHECK (octet_length(publisher_id) BETWEEN 1 AND 128), \
    idem_key text NOT NULL CHECK (octet_length(idem_key) BETWEEN 1 AND 128), \
    fingerprint bytea NOT NULL CHECK (octet_length(fingerprint) = 32), \
    review_id text REFERENCES spot_control.spot_store_state_review (review_id), \
    created_at bigint NOT NULL, \
    PRIMARY KEY (publisher_id, idem_key)\
); \
CREATE INDEX IF NOT EXISTS spot_store_state_idempotency_created_idx \
    ON spot_control.spot_store_state_idempotency (created_at); \
CREATE TABLE IF NOT EXISTS spot_control.spot_store_state_revoked_key (\
    key_id text PRIMARY KEY CHECK (octet_length(key_id) BETWEEN 1 AND 128), \
    reason text NOT NULL CHECK (octet_length(reason) <= 1024), \
    at bigint NOT NULL\
); \
CREATE TABLE IF NOT EXISTS spot_control.spot_store_state_revoked_bundle (\
    digest text PRIMARY KEY, \
    reason text NOT NULL CHECK (octet_length(reason) <= 1024), \
    at bigint NOT NULL\
); \
CREATE TABLE IF NOT EXISTS spot_control.spot_store_state_issued_list (\
    id smallint PRIMARY KEY CHECK (id = 1), \
    generation bigint NOT NULL CHECK (generation >= 0), \
    issued_at bigint NOT NULL, \
    next_update bigint NOT NULL, \
    body text NOT NULL CHECK (octet_length(body) BETWEEN 1 AND 1048576)\
); \
CREATE TABLE IF NOT EXISTS spot_control.spot_store_state_audit (\
    id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY, \
    kind text NOT NULL CHECK (kind IN ('publish', 'yank', 'withdraw', 'revoke_publisher', \
        'revoke_key', 'revoke_bundle', 'approve', 'reject', 'key_issue', 'key_revoke')), \
    actor text NOT NULL CHECK (octet_length(actor) BETWEEN 1 AND 128), \
    publisher_id text CHECK (octet_length(publisher_id) BETWEEN 1 AND 128), \
    app text, \
    version text, \
    digest text, \
    reason text NOT NULL CHECK (octet_length(reason) <= 1024), \
    at bigint NOT NULL\
); \
CREATE OR REPLACE FUNCTION spot_control.spot_store_state_audit_immutable() \
RETURNS trigger LANGUAGE plpgsql AS $fn$ \
BEGIN \
    RAISE EXCEPTION 'the store audit log is append-only'; \
END \
$fn$; \
DROP TRIGGER IF EXISTS spot_store_state_audit_append_only ON spot_control.spot_store_state_audit; \
CREATE TRIGGER spot_store_state_audit_append_only \
    BEFORE UPDATE OR DELETE ON spot_control.spot_store_state_audit \
    FOR EACH ROW EXECUTE FUNCTION spot_control.spot_store_state_audit_immutable();";

/// Creates the tables in one transaction under a transaction-scoped advisory lock.
///
/// # Errors
/// The driver's error; the caller maps it to a fixed sentence.
pub(crate) async fn prepare(client: &mut Client) -> Result<(), tokio_postgres::Error> {
    let tx = client.transaction().await?;
    tx.execute("SELECT pg_advisory_xact_lock($1)", &[&SCHEMA_LOCK])
        .await?;
    tx.batch_execute(DDL).await?;
    tx.commit().await
}
