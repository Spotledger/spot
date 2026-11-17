//! The result table of the scheduled audit.
//!
//! `spot_journal_audit_result`: one row per audit run of the site's own tenant (one tenant, one
//! database, ADR-34). A row holds counts, positions, the duration, the outcome and, for a
//! mismatch, the **identity** of the first differing record (entity, id) and the kind of
//! difference: never a field value. Fifty-two weeks are kept ([`RETENTION_MILLIS`]); the sink
//! prunes older rows when it records.

/// The idempotent DDL: one statement per object, each an `IF NOT EXISTS` creation, created by
/// `spot migrate` through the extra-table hook.
pub const DDL: &str = r#"
CREATE TABLE IF NOT EXISTS spot_journal_audit_result (
    id                bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    tenant            text NOT NULL,
    started_millis    bigint NOT NULL,
    finished_millis   bigint NOT NULL,
    outcome           text NOT NULL CHECK (outcome IN ('clean', 'mismatch', 'failed')),
    failure_stage     text,
    failure_code      text,
    verified_generation bigint,
    verified_seq      bigint,
    records_live      bigint,
    records_rebuilt   bigint,
    differences       bigint,
    first_entity      text,
    first_id          text,
    first_kind        text,
    rebuild_millis    bigint,
    rto_breached      boolean,
    CONSTRAINT spot_journal_audit_result_tenant CHECK (length(tenant) BETWEEN 1 AND 128)
);
CREATE INDEX IF NOT EXISTS spot_journal_audit_result_tenant_idx ON spot_journal_audit_result (tenant, started_millis);
"#;

/// Fifty-two weeks, in milliseconds.
pub const RETENTION_MILLIS: i64 = 52 * 7 * 24 * 60 * 60 * 1000;
