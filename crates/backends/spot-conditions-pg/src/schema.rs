//! The DDL of this crate's two tables (WP-RTB-01.T2, ADR-067 sections 1 and 6).
//!
//! | Table | Holds |
//! | --- | --- |
//! | `spot_condition_open` | one row per open condition, keyed by `(id, subject)`: the only durable record of what is open after a restart (`load_open`) |
//! | `spot_condition_history` | one row per recorded transition, append-only, bounded per subject to the newest 1,000 rows and 90 days |
//!
//! One database is one site (ADR-34): the tables carry no site column, and a condition of one
//! site is never visible in another's rows because it never shares a database with one.
//!
//! The parameters and the subject are the engine's own validated ids and counts (`spot-conditions`
//! refuses anything else before it reaches here); this crate stores them as given and never
//! formats a record's content into them.

/// The idempotent DDL. Every statement is `IF NOT EXISTS`.
pub const DDL: &str = r#"
CREATE TABLE IF NOT EXISTS spot_condition_open (
    id                text NOT NULL,
    subject           text NOT NULL,
    severity          text NOT NULL,
    params            jsonb NOT NULL,
    raised_at_millis  bigint NOT NULL CHECK (raised_at_millis >= 0),
    last_seen_millis  bigint NOT NULL CHECK (last_seen_millis >= 0),
    PRIMARY KEY (id, subject)
);

CREATE TABLE IF NOT EXISTS spot_condition_history (
    seq         bigserial PRIMARY KEY,
    id          text NOT NULL,
    subject     text NOT NULL,
    transition  text NOT NULL CHECK (transition IN ('raised', 'escalated', 'cleared')),
    severity    text NOT NULL,
    params      jsonb NOT NULL,
    at_millis   bigint NOT NULL CHECK (at_millis >= 0),
    recorded_at timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS spot_condition_history_subject_idx
    ON spot_condition_history (subject, seq DESC);
"#;

/// Most history rows kept per subject; the oldest beyond this are pruned after every write
/// ([`crate::store::PgConditionStore::apply`]).
pub const HISTORY_MAX_ROWS_PER_SUBJECT: i64 = 1_000;

/// Longest a history row is kept, regardless of how many a subject has, in days.
pub const HISTORY_RETENTION_DAYS: i64 = 90;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_statement_is_idempotent() {
        for statement in DDL.split(';').map(str::trim).filter(|s| !s.is_empty()) {
            let head = statement.to_ascii_uppercase();
            assert!(
                head.starts_with("CREATE TABLE IF NOT EXISTS")
                    || head.starts_with("CREATE INDEX IF NOT EXISTS"),
                "not idempotent: {statement}"
            );
        }
    }

    #[test]
    fn every_table_carries_the_spot_prefix() {
        for table in ["spot_condition_open", "spot_condition_history"] {
            assert!(DDL.contains(table));
            assert!(table.starts_with("spot_"));
        }
    }
}
