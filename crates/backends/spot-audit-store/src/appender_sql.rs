//! The tables and statements of the durable appender state (WP-AI-11.T5).
//!
//! The same shape as the chain tables of [`crate::TABLE_DDL`]: one `CREATE TABLE IF NOT EXISTS`
//! per table, in the form `spot_pg_migrate::ExtraTable` accepts, so the migrator plans them and
//! nothing else creates them. Every table carries the site in its primary key. Values reach SQL
//! only as bound parameters; every identifier here is a constant of this crate (ADR-45).

/// The per-lane outbox bookmark: `(site, lane)` to the last position the lane scanned.
pub const BOOKMARK_TABLE: &str = "spot_audit_bookmark";

/// The event-id index: `(site, event_id)` to the `(lane, seq)` the event was chained at.
pub const EVENT_INDEX_TABLE: &str = "spot_audit_event_index";

/// The per-document index: every `(lane, seq)` entry of a document, in append order.
pub const DOC_INDEX_TABLE: &str = "spot_audit_doc_index";

/// The newest signed site checkpoint, one row per site.
pub const SITE_CHECKPOINT_TABLE: &str = "spot_audit_site_checkpoint";

/// The bookmark table. `generation` and `seq` are the two halves of an outbox `FeedPosition`.
pub const BOOKMARK_TABLE_DDL: &str = "\
CREATE TABLE IF NOT EXISTS spot_audit_bookmark (\
    site text NOT NULL, \
    lane bigint NOT NULL, \
    generation bigint NOT NULL, \
    seq bigint NOT NULL, \
    PRIMARY KEY (site, lane))";

/// The event-id index table.
pub const EVENT_INDEX_TABLE_DDL: &str = "\
CREATE TABLE IF NOT EXISTS spot_audit_event_index (\
    site text NOT NULL, \
    event_id text NOT NULL, \
    lane bigint NOT NULL, \
    seq bigint NOT NULL, \
    PRIMARY KEY (site, event_id))";

/// The document index table. `ord` records the order the entries were recorded in, which is the
/// append order the document history is read in.
pub const DOC_INDEX_TABLE_DDL: &str = "\
CREATE TABLE IF NOT EXISTS spot_audit_doc_index (\
    site text NOT NULL, \
    doctype text NOT NULL, \
    id text NOT NULL, \
    lane bigint NOT NULL, \
    seq bigint NOT NULL, \
    ord bigint GENERATED ALWAYS AS IDENTITY, \
    PRIMARY KEY (site, doctype, id, lane, seq))";

/// The site checkpoint table. `body` is the checkpoint as JSON; `at` is its signing time, so an
/// older checkpoint can never replace a newer one.
pub const SITE_CHECKPOINT_TABLE_DDL: &str = "\
CREATE TABLE IF NOT EXISTS spot_audit_site_checkpoint (\
    site text PRIMARY KEY, \
    at bigint NOT NULL, \
    body text NOT NULL)";

/// Each appender table as `(marker table name, statement)`, in creation order: what a host hands
/// the migrator as one `ExtraTable` per pair, beside [`crate::TABLE_DDL`].
pub const APPENDER_TABLE_DDL: [(&str, &str); 4] = [
    (BOOKMARK_TABLE, BOOKMARK_TABLE_DDL),
    (EVENT_INDEX_TABLE, EVENT_INDEX_TABLE_DDL),
    (DOC_INDEX_TABLE, DOC_INDEX_TABLE_DDL),
    (SITE_CHECKPOINT_TABLE, SITE_CHECKPOINT_TABLE_DDL),
];

/// Each appender table with the column names it must have (sorted), for the presence check.
pub(crate) const APPENDER_COLUMNS: [(&str, &[&str]); 4] = [
    (BOOKMARK_TABLE, &["generation", "lane", "seq", "site"]),
    (EVENT_INDEX_TABLE, &["event_id", "lane", "seq", "site"]),
    (
        DOC_INDEX_TABLE,
        &["doctype", "id", "lane", "ord", "seq", "site"],
    ),
    (SITE_CHECKPOINT_TABLE, &["at", "body", "site"]),
];

/// A lane's bookmark.
pub(crate) const BOOKMARK_GET: &str =
    "SELECT generation, seq FROM spot_audit_bookmark WHERE site = $1 AND lane = $2";

/// Sets a lane's bookmark; a repeated call with the same value writes nothing.
pub(crate) const BOOKMARK_SET: &str = "\
INSERT INTO spot_audit_bookmark (site, lane, generation, seq) VALUES ($1, $2, $3, $4) \
ON CONFLICT (site, lane) DO UPDATE SET generation = EXCLUDED.generation, seq = EXCLUDED.seq \
WHERE (spot_audit_bookmark.generation, spot_audit_bookmark.seq) \
      IS DISTINCT FROM (EXCLUDED.generation, EXCLUDED.seq)";

/// Where an event id was chained.
pub(crate) const EVENT_GET: &str =
    "SELECT lane, seq FROM spot_audit_event_index WHERE site = $1 AND event_id = $2";

/// Records an event id; an existing row is never overwritten (the caller compares it).
pub(crate) const EVENT_INSERT: &str = "\
INSERT INTO spot_audit_event_index (site, event_id, lane, seq) VALUES ($1, $2, $3, $4) \
ON CONFLICT (site, event_id) DO NOTHING";

/// A document's entries, in the order they were recorded.
pub(crate) const DOC_LIST: &str = "\
SELECT lane, seq FROM spot_audit_doc_index \
WHERE site = $1 AND doctype = $2 AND id = $3 ORDER BY ord";

/// Records one document entry; a repeated call changes nothing.
pub(crate) const DOC_INSERT: &str = "\
INSERT INTO spot_audit_doc_index (site, doctype, id, lane, seq) VALUES ($1, $2, $3, $4, $5) \
ON CONFLICT (site, doctype, id, lane, seq) DO NOTHING";

/// The newest site checkpoint.
pub(crate) const CHECKPOINT_GET: &str =
    "SELECT body FROM spot_audit_site_checkpoint WHERE site = $1";

/// Stores a site checkpoint unless the stored one is newer.
pub(crate) const CHECKPOINT_SET: &str = "\
INSERT INTO spot_audit_site_checkpoint (site, at, body) VALUES ($1, $2, $3) \
ON CONFLICT (site) DO UPDATE SET at = EXCLUDED.at, body = EXCLUDED.body \
WHERE spot_audit_site_checkpoint.at <= EXCLUDED.at";

/// The columns of one table of the current schema, sorted.
pub(crate) const COLUMNS_OF: &str = "\
SELECT column_name::text FROM information_schema.columns \
WHERE table_schema = current_schema() AND table_name = $1 ORDER BY column_name";
