//! The DDL of this crate's one table (WP-JRN-02.T3, ADR-061 s8).
//!
//! `spot_capture_watermark` holds one row per stream: where the capture invariant has settled.
//! `seq` means "every entry of that stream at or below this position was compared definitively
//! and no open finding names it". The host's `schema::extra_tables` carries [`DDL`], so
//! `spot migrate` creates the table and the serving host never does.
//!
//! One database is one site (ADR-34): there is no tenant column.

/// The idempotent DDL: one statement, `IF NOT EXISTS`.
pub const DDL: &str = r#"
CREATE TABLE IF NOT EXISTS spot_capture_watermark (
    source     text PRIMARY KEY CHECK (source IN ('change_feed', 'doc_state')),
    generation bigint NOT NULL CHECK (generation >= 0),
    seq        bigint NOT NULL CHECK (seq >= 0),
    updated_at timestamptz NOT NULL
);
"#;

/// The tables the DDL creates, for a presence check.
pub const TABLES: [&str; 1] = ["spot_capture_watermark"];
