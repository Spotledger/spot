//! The SQL of the command ledger. Everything here is a constant: no value is ever interpolated
//! (ADR-45).

/// Bump when [`SCHEMA`] changes; `install` runs it when the stored version differs.
pub(super) const SCHEMA_VERSION: &str = "1";

/// Key of the schema version in `spot_meta`.
pub(super) const META_SCHEMA: &str = "command_ledger_schema";

/// The table of command records. One database is one tenant, so the site is implicit. The
/// outcome is `jsonb` and holds identity and messages only (at most 16 KiB, checked by
/// `CommandRecord::validate`); `retain_until` is the row's own retention date, written by the
/// engine from the policy of the caller's class, and the only column pruning looks at.
pub(super) const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS spot_command_ledger (
    principal text COLLATE "C" NOT NULL,
    command_id text COLLATE "C" NOT NULL,
    fingerprint bytea NOT NULL,
    outcome jsonb NOT NULL,
    recorded_at timestamptz NOT NULL,
    retain_until timestamptz NOT NULL,
    PRIMARY KEY (principal, command_id)
);
CREATE INDEX IF NOT EXISTS spot_command_ledger_retain_until
    ON spot_command_ledger (retain_until);
"#;

/// Reads one record. `outcome` is read back as text.
pub(super) const GET: &str = "SELECT fingerprint, outcome::text, recorded_at, retain_until \
     FROM spot_command_ledger WHERE principal = $1 AND command_id = $2";

/// Inserts one record in the caller's transaction. A second insert of the key fails with
/// SQLSTATE 23505 (`UniqueViolation`). The outcome travels as text and is cast to `jsonb`.
pub(super) const PUT: &str = "INSERT INTO spot_command_ledger \
     (principal, command_id, fingerprint, outcome, recorded_at, retain_until) \
     VALUES ($1, $2, $3, $4::text::jsonb, $5, $6)";

/// Deletes by `retain_until` only, never by `recorded_at`.
pub(super) const PRUNE: &str = "DELETE FROM spot_command_ledger WHERE retain_until <= $1";
