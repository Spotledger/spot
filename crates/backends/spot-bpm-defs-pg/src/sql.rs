//! The statements of the store. Values reach SQL only as bound parameters (ADR-045); nothing is
//! interpolated.

/// The columns of a stored version, in the order `store::row` reads them.
pub const SELECT_VERSION: &str = "\
SELECT name, version, text, content_hash, state, published_by, published_at, note, \
       retired_at, retired_by \
FROM spot_bpm_definition WHERE tenant = $1 AND name = $2 AND version = $3";

/// The highest version still published.
pub const SELECT_LATEST_PUBLISHED: &str = "\
SELECT name, version, text, content_hash, state, published_by, published_at, note, \
       retired_at, retired_by \
FROM spot_bpm_definition WHERE tenant = $1 AND name = $2 AND state = 'published' \
ORDER BY version DESC LIMIT 1";

/// A page of versions above `$3`, ascending.
pub const SELECT_PAGE: &str = "\
SELECT name, version, text, content_hash, state, published_by, published_at, note, \
       retired_at, retired_by \
FROM spot_bpm_definition WHERE tenant = $1 AND name = $2 AND version > $3 \
ORDER BY version LIMIT $4";

/// Every process name of the tenant.
pub const SELECT_NAMES: &str = "\
SELECT name FROM spot_bpm_definition WHERE tenant = $1 GROUP BY name \
ORDER BY name COLLATE \"C\"";

/// The head row, created at zero when missing (rolled back with a refused publish).
pub const ENSURE_HEAD: &str = "\
INSERT INTO spot_bpm_definition_head (tenant, name, latest) VALUES ($1, $2, 0) \
ON CONFLICT (tenant, name) DO NOTHING";

/// The atomic step: take the next number only if the head still holds the expected one.
pub const BUMP_HEAD: &str = "\
UPDATE spot_bpm_definition_head SET latest = latest + 1 \
WHERE tenant = $1 AND name = $2 AND latest = $3 RETURNING latest";

/// The head, to report what a conflicting publish found.
pub const SELECT_HEAD: &str = "\
SELECT latest FROM spot_bpm_definition_head WHERE tenant = $1 AND name = $2";

/// A fresh published version.
pub const INSERT_VERSION: &str = "\
INSERT INTO spot_bpm_definition \
    (tenant, name, version, text, content_hash, state, published_by, published_at, note) \
VALUES ($1, $2, $3, $4, $5, 'published', $6, $7, $8)";

/// Retires a published version and returns it; no row when absent or already retired.
pub const RETIRE: &str = "\
UPDATE spot_bpm_definition SET state = 'retired', retired_at = $4, retired_by = $5 \
WHERE tenant = $1 AND name = $2 AND version = $3 AND state = 'published' \
RETURNING name, version, text, content_hash, state, published_by, published_at, note, \
          retired_at, retired_by";

/// Whether the Case table exists in the current schema.
pub const CASE_TABLE_EXISTS: &str = "\
SELECT to_regclass(quote_ident(current_schema()) || '.spot_bpm_case') IS NOT NULL";

/// The versions running Cases of the tenant are pinned to.
pub const SELECT_PINS: &str = "\
SELECT DISTINCT dag_version FROM spot_bpm_case \
WHERE tenant = $1 AND dag_key = $2 AND status = 'running' ORDER BY dag_version";
