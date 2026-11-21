//! The control-schema tables, created idempotently.
//!
//! | Table | Holds |
//! | --- | --- |
//! | `spot_control.spot_registry_meta` | one row: the index revision, bumped by every catalogue-visible mutation (a search cursor is bound to it) |
//! | `spot_control.spot_registry_publisher` | publishers and their administrative status |
//! | `spot_control.spot_registry_ownership` | app name primary key, the owning publisher and the transfer generation |
//! | `spot_control.spot_registry_release` | one row per (app, version): digest (unique), state (yanked and withdrawn markers with reasons), visibility flags, capability flags, the search tokens and the signed evidence as `jsonb` |
//! | `spot_control.spot_registry_transfer` | the dual-evidence transfer history, append-only |
//!
//! Plain PostgreSQL: no extension is required. Searching narrows with the `search_tokens` array
//! (GIN index) and then applies the contract's ranking in Rust; see [`crate`].

use tokio_postgres::Client;

/// The control schema, shared with `spot-tenancy-pg`.
pub const CONTROL_SCHEMA: &str = "spot_control";

const SCHEMA_LOCK: i64 = 0x5245_4749_5301;

/// The idempotent DDL.
pub const DDL: &str = "\
CREATE SCHEMA IF NOT EXISTS spot_control; \
CREATE TABLE IF NOT EXISTS spot_control.spot_registry_meta (\
    id smallint PRIMARY KEY CHECK (id = 1), \
    revision bigint NOT NULL CHECK (revision >= 0)\
); \
INSERT INTO spot_control.spot_registry_meta (id, revision) VALUES (1, 0) ON CONFLICT (id) DO NOTHING; \
CREATE TABLE IF NOT EXISTS spot_control.spot_registry_publisher (\
    id text PRIMARY KEY, \
    display_name text NOT NULL, \
    root_key text NOT NULL, \
    status text NOT NULL CHECK (status IN ('active', 'suspended'))\
); \
CREATE TABLE IF NOT EXISTS spot_control.spot_registry_ownership (\
    app text PRIMARY KEY, \
    publisher text NOT NULL REFERENCES spot_control.spot_registry_publisher (id), \
    generation bigint NOT NULL CHECK (generation >= 0)\
); \
CREATE TABLE IF NOT EXISTS spot_control.spot_registry_release (\
    seq bigint GENERATED ALWAYS AS IDENTITY UNIQUE, \
    app text NOT NULL, \
    version text NOT NULL, \
    digest text NOT NULL CONSTRAINT spot_registry_release_digest_key UNIQUE, \
    size bigint NOT NULL CHECK (size >= 0), \
    requires_host text, \
    published_at bigint NOT NULL, \
    publisher text NOT NULL REFERENCES spot_control.spot_registry_publisher (id), \
    yanked_at bigint, \
    yanked_reason text, \
    withdrawn_at bigint, \
    withdrawn_reason text, \
    title text NOT NULL, \
    description text NOT NULL, \
    category text NOT NULL, \
    category_key text NOT NULL, \
    is_pre boolean NOT NULL, \
    cap_network boolean NOT NULL, \
    cap_email boolean NOT NULL, \
    cap_files text NOT NULL CHECK (cap_files IN ('none', 'read', 'read_write')), \
    cap_doc_read boolean NOT NULL, \
    cap_doc_write boolean NOT NULL, \
    evidence jsonb NOT NULL CHECK (jsonb_typeof(evidence) = 'object'), \
    search_tokens text[] NOT NULL, \
    search_vector tsvector NOT NULL, \
    PRIMARY KEY (app, version), \
    CHECK ((yanked_at IS NULL) = (yanked_reason IS NULL)), \
    CHECK ((withdrawn_at IS NULL) = (withdrawn_reason IS NULL))\
); \
CREATE INDEX IF NOT EXISTS spot_registry_release_tokens_idx \
    ON spot_control.spot_registry_release USING gin (search_tokens); \
CREATE INDEX IF NOT EXISTS spot_registry_release_vector_idx \
    ON spot_control.spot_registry_release USING gin (search_vector); \
CREATE INDEX IF NOT EXISTS spot_registry_release_publisher_idx \
    ON spot_control.spot_registry_release (publisher); \
CREATE TABLE IF NOT EXISTS spot_control.spot_registry_transfer (\
    seq bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY, \
    app text NOT NULL, \
    from_publisher text NOT NULL, \
    to_publisher text NOT NULL, \
    generation bigint NOT NULL CHECK (generation >= 0), \
    at bigint NOT NULL, \
    expires_at bigint NOT NULL, \
    reason text NOT NULL, \
    evidence jsonb NOT NULL CHECK (jsonb_typeof(evidence) = 'object')\
); \
CREATE INDEX IF NOT EXISTS spot_registry_transfer_app_idx \
    ON spot_control.spot_registry_transfer (app, seq); \
CREATE OR REPLACE FUNCTION spot_control.spot_registry_transfer_immutable() \
RETURNS trigger LANGUAGE plpgsql AS $fn$ \
BEGIN \
    RAISE EXCEPTION 'the registry transfer history is append-only'; \
END \
$fn$; \
DROP TRIGGER IF EXISTS spot_registry_transfer_append_only ON spot_control.spot_registry_transfer; \
CREATE TRIGGER spot_registry_transfer_append_only \
    BEFORE UPDATE OR DELETE ON spot_control.spot_registry_transfer \
    FOR EACH ROW EXECUTE FUNCTION spot_control.spot_registry_transfer_immutable();";

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
