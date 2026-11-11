//! The DDL of the access review and SoD tables.
//!
//! | Table | Holds |
//! | --- | --- |
//! | `spot_access_review_campaigns` | one row per (tenant, campaign): the spec and opening events (immutable) and the status |
//! | `spot_access_review_items` | one row per item: the snapshot (immutable), the state and its version |
//! | `spot_access_review_history` | append-only: events, removal actions and refusals, numbered per campaign |
//! | `spot_sod_rule_sets` | append-only: every version of a tenant's rule set, as validated TOML |
//! | `spot_sod_overrides` | append-only: every override exactly as granted, numbered per tenant |
//! | `spot_sod_override_revocations` | append-only: at most one revocation per override |
//!
//! Immutability is enforced by the database, not by the application: triggers refuse any
//! UPDATE, DELETE or TRUNCATE of the append-only tables (the owner included), refuse changes
//! to an item's snapshot and to a campaign's spec, and allow an item's version to move by
//! exactly one. The application role `spot_access_review_app` (created when the connection
//! may create roles) is granted no UPDATE or DELETE on the append-only tables at all.
//!
//! [`prepare`] is idempotent and serialised across nodes by a transaction-scoped advisory lock.

use deadpool_postgres::Pool;
use spot_app_api::AppResult;

use crate::error::{migration, migration_pool};

/// The lock key that serialises concurrent `prepare` calls (transaction scoped).
const PREPARE_LOCK: i64 = 0x5353_4f50_0003;

/// The idempotent DDL. Every statement can run again.
pub const DDL: &str = r#"
CREATE TABLE IF NOT EXISTS spot_access_review_campaigns (
    tenant      text    NOT NULL,
    campaign_id text    NOT NULL,
    title       text    NOT NULL,
    deadline    bigint  NOT NULL,
    item_count  integer NOT NULL,
    spec        text    NOT NULL,
    open_events text    NOT NULL,
    status      text    NOT NULL,
    opened_at   bigint  NOT NULL,
    PRIMARY KEY (tenant, campaign_id),
    CONSTRAINT spot_access_review_campaigns_ck CHECK (
        status IN ('open', 'completed')
        AND octet_length(tenant) BETWEEN 1 AND 64
        AND octet_length(campaign_id) BETWEEN 1 AND 128
        AND octet_length(title) <= 4096
        AND item_count BETWEEN 1 AND 10000)
);

CREATE TABLE IF NOT EXISTS spot_access_review_items (
    tenant      text    NOT NULL,
    campaign_id text    NOT NULL,
    ord         integer NOT NULL,
    item_id     text    NOT NULL,
    snapshot    text    NOT NULL,
    state       text    NOT NULL,
    version     bigint  NOT NULL DEFAULT 0,
    PRIMARY KEY (tenant, campaign_id, item_id),
    UNIQUE (tenant, campaign_id, ord),
    FOREIGN KEY (tenant, campaign_id)
        REFERENCES spot_access_review_campaigns (tenant, campaign_id),
    CONSTRAINT spot_access_review_items_ck CHECK (
        version >= 0 AND ord >= 0 AND octet_length(item_id) BETWEEN 1 AND 128)
);

CREATE TABLE IF NOT EXISTS spot_access_review_history (
    tenant      text   NOT NULL,
    campaign_id text   NOT NULL,
    seq         bigint NOT NULL,
    kind        text   NOT NULL,
    entry       text   NOT NULL,
    at          bigint NOT NULL,
    PRIMARY KEY (tenant, campaign_id, seq),
    FOREIGN KEY (tenant, campaign_id)
        REFERENCES spot_access_review_campaigns (tenant, campaign_id),
    CONSTRAINT spot_access_review_history_ck CHECK (
        kind IN ('event', 'action', 'refused') AND seq >= 1)
);

CREATE TABLE IF NOT EXISTS spot_sod_rule_sets (
    tenant  text   NOT NULL,
    version bigint NOT NULL,
    toml    text   NOT NULL,
    set_by  text   NOT NULL,
    set_at  bigint NOT NULL,
    PRIMARY KEY (tenant, version),
    CONSTRAINT spot_sod_rule_sets_ck CHECK (
        version >= 1 AND octet_length(toml) <= 1048576
        AND octet_length(set_by) BETWEEN 1 AND 128)
);

CREATE TABLE IF NOT EXISTS spot_sod_overrides (
    tenant             text   NOT NULL,
    seq                bigint NOT NULL,
    rule_id            text   NOT NULL,
    user_id            text   NOT NULL,
    scope              text,
    reason             text   NOT NULL,
    mitigating_control text   NOT NULL,
    granted_by         text   NOT NULL,
    granted_at         bigint NOT NULL,
    expires_at         bigint NOT NULL,
    PRIMARY KEY (tenant, seq),
    CONSTRAINT spot_sod_overrides_ck CHECK (
        seq >= 1 AND expires_at > granted_at
        AND btrim(reason) <> '' AND btrim(mitigating_control) <> ''
        AND granted_by <> user_id
        AND octet_length(rule_id) BETWEEN 1 AND 128
        AND octet_length(user_id) BETWEEN 1 AND 128
        AND octet_length(reason) <= 4096
        AND octet_length(mitigating_control) <= 4096)
);

CREATE INDEX IF NOT EXISTS spot_sod_overrides_cover_idx
    ON spot_sod_overrides (tenant, rule_id, user_id);

CREATE TABLE IF NOT EXISTS spot_sod_override_revocations (
    tenant     text   NOT NULL,
    seq        bigint NOT NULL,
    revoked_by text   NOT NULL,
    reason     text   NOT NULL,
    revoked_at bigint NOT NULL,
    PRIMARY KEY (tenant, seq),
    FOREIGN KEY (tenant, seq) REFERENCES spot_sod_overrides (tenant, seq),
    CONSTRAINT spot_sod_override_revocations_ck CHECK (
        btrim(reason) <> '' AND octet_length(reason) <= 4096
        AND octet_length(revoked_by) BETWEEN 1 AND 128)
);

CREATE OR REPLACE FUNCTION spot_access_review_append_only() RETURNS trigger AS $$
BEGIN
    RAISE EXCEPTION '% is append-only', TG_TABLE_NAME USING ERRCODE = '42501';
END $$ LANGUAGE plpgsql;

CREATE OR REPLACE FUNCTION spot_access_review_campaign_guard() RETURNS trigger AS $$
BEGIN
    IF TG_OP = 'UPDATE'
       AND NEW.tenant = OLD.tenant AND NEW.campaign_id = OLD.campaign_id
       AND NEW.title = OLD.title AND NEW.deadline = OLD.deadline
       AND NEW.item_count = OLD.item_count AND NEW.spec = OLD.spec
       AND NEW.open_events = OLD.open_events AND NEW.opened_at = OLD.opened_at
       AND NOT (OLD.status = 'completed' AND NEW.status <> 'completed') THEN
        RETURN NEW;
    END IF;
    RAISE EXCEPTION 'a campaign is immutable except for its status' USING ERRCODE = '42501';
END $$ LANGUAGE plpgsql;

CREATE OR REPLACE FUNCTION spot_access_review_item_guard() RETURNS trigger AS $$
BEGIN
    IF TG_OP = 'UPDATE'
       AND NEW.tenant = OLD.tenant AND NEW.campaign_id = OLD.campaign_id
       AND NEW.item_id = OLD.item_id AND NEW.ord = OLD.ord
       AND NEW.snapshot = OLD.snapshot AND NEW.version = OLD.version + 1 THEN
        RETURN NEW;
    END IF;
    RAISE EXCEPTION 'an item snapshot is immutable and its version moves by one'
        USING ERRCODE = '42501';
END $$ LANGUAGE plpgsql;

DROP TRIGGER IF EXISTS spot_access_review_history_rows ON spot_access_review_history;
CREATE TRIGGER spot_access_review_history_rows
    BEFORE UPDATE OR DELETE ON spot_access_review_history
    FOR EACH ROW EXECUTE FUNCTION spot_access_review_append_only();
DROP TRIGGER IF EXISTS spot_access_review_history_truncate ON spot_access_review_history;
CREATE TRIGGER spot_access_review_history_truncate
    BEFORE TRUNCATE ON spot_access_review_history
    FOR EACH STATEMENT EXECUTE FUNCTION spot_access_review_append_only();

DROP TRIGGER IF EXISTS spot_access_review_campaigns_rows ON spot_access_review_campaigns;
CREATE TRIGGER spot_access_review_campaigns_rows
    BEFORE UPDATE OR DELETE ON spot_access_review_campaigns
    FOR EACH ROW EXECUTE FUNCTION spot_access_review_campaign_guard();
DROP TRIGGER IF EXISTS spot_access_review_campaigns_truncate ON spot_access_review_campaigns;
CREATE TRIGGER spot_access_review_campaigns_truncate
    BEFORE TRUNCATE ON spot_access_review_campaigns
    FOR EACH STATEMENT EXECUTE FUNCTION spot_access_review_append_only();

DROP TRIGGER IF EXISTS spot_access_review_items_rows ON spot_access_review_items;
CREATE TRIGGER spot_access_review_items_rows
    BEFORE UPDATE OR DELETE ON spot_access_review_items
    FOR EACH ROW EXECUTE FUNCTION spot_access_review_item_guard();
DROP TRIGGER IF EXISTS spot_access_review_items_truncate ON spot_access_review_items;
CREATE TRIGGER spot_access_review_items_truncate
    BEFORE TRUNCATE ON spot_access_review_items
    FOR EACH STATEMENT EXECUTE FUNCTION spot_access_review_append_only();

DROP TRIGGER IF EXISTS spot_sod_rule_sets_rows ON spot_sod_rule_sets;
CREATE TRIGGER spot_sod_rule_sets_rows
    BEFORE UPDATE OR DELETE ON spot_sod_rule_sets
    FOR EACH ROW EXECUTE FUNCTION spot_access_review_append_only();
DROP TRIGGER IF EXISTS spot_sod_rule_sets_truncate ON spot_sod_rule_sets;
CREATE TRIGGER spot_sod_rule_sets_truncate
    BEFORE TRUNCATE ON spot_sod_rule_sets
    FOR EACH STATEMENT EXECUTE FUNCTION spot_access_review_append_only();

DROP TRIGGER IF EXISTS spot_sod_overrides_rows ON spot_sod_overrides;
CREATE TRIGGER spot_sod_overrides_rows
    BEFORE UPDATE OR DELETE ON spot_sod_overrides
    FOR EACH ROW EXECUTE FUNCTION spot_access_review_append_only();
DROP TRIGGER IF EXISTS spot_sod_overrides_truncate ON spot_sod_overrides;
CREATE TRIGGER spot_sod_overrides_truncate
    BEFORE TRUNCATE ON spot_sod_overrides
    FOR EACH STATEMENT EXECUTE FUNCTION spot_access_review_append_only();

DROP TRIGGER IF EXISTS spot_sod_override_revocations_rows ON spot_sod_override_revocations;
CREATE TRIGGER spot_sod_override_revocations_rows
    BEFORE UPDATE OR DELETE ON spot_sod_override_revocations
    FOR EACH ROW EXECUTE FUNCTION spot_access_review_append_only();
DROP TRIGGER IF EXISTS spot_sod_override_revocations_truncate ON spot_sod_override_revocations;
CREATE TRIGGER spot_sod_override_revocations_truncate
    BEFORE TRUNCATE ON spot_sod_override_revocations
    FOR EACH STATEMENT EXECUTE FUNCTION spot_access_review_append_only();
"#;

/// The application role and its grants: read and append on the append-only tables, no UPDATE
/// or DELETE; update on the two mutable tables, no DELETE. Skipped when the connection may not
/// create roles (the triggers above hold either way).
pub const ROLE: &str = r#"
DO $$ BEGIN
    IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'spot_access_review_app') THEN
        CREATE ROLE spot_access_review_app NOLOGIN;
    END IF;
EXCEPTION WHEN duplicate_object OR unique_violation OR insufficient_privilege THEN NULL;
END $$;

DO $$ BEGIN
    IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'spot_access_review_app') THEN
        EXECUTE format('GRANT USAGE ON SCHEMA %I TO spot_access_review_app', current_schema());
        REVOKE ALL ON spot_access_review_campaigns, spot_access_review_items,
            spot_access_review_history, spot_sod_rule_sets, spot_sod_overrides,
            spot_sod_override_revocations FROM spot_access_review_app;
        GRANT SELECT, INSERT, UPDATE ON spot_access_review_campaigns, spot_access_review_items
            TO spot_access_review_app;
        GRANT SELECT, INSERT ON spot_access_review_history, spot_sod_rule_sets,
            spot_sod_overrides, spot_sod_override_revocations TO spot_access_review_app;
    END IF;
EXCEPTION WHEN insufficient_privilege THEN NULL;
END $$;
"#;

/// Creates the tables, guards and role if missing. Idempotent and safe from several nodes at
/// once (a transaction-scoped advisory lock serialises them).
///
/// # Errors
/// `access_review_pg.migration_failed` when the database cannot apply the DDL. The message
/// carries a SQLSTATE or a fixed sentence, never a row value.
pub async fn prepare(pool: &Pool) -> AppResult<()> {
    let mut conn = pool.get().await.map_err(|e| migration_pool(&e))?;
    let tx = conn.transaction().await.map_err(|e| migration(&e))?;
    tx.execute("SELECT pg_advisory_xact_lock($1)", &[&PREPARE_LOCK])
        .await
        .map_err(|e| migration(&e))?;
    tx.batch_execute(DDL).await.map_err(|e| migration(&e))?;
    tx.batch_execute(ROLE).await.map_err(|e| migration(&e))?;
    tx.commit().await.map_err(|e| migration(&e))
}
