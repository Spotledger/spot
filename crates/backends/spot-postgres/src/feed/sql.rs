//! The SQL of the feed: tables, the capture functions and the statements that publish and prune.
//!
//! Everything here is a constant. Identifiers are fixed names (prefix `spot_`); no value is ever
//! interpolated (ADR-45). `xid8` columns are never sent to or read by the driver: they are only
//! compared and assigned inside statements.

/// Bump when [`SCHEMA`] changes; `install` runs it when the stored version differs. Changes are
/// additive (`CREATE ... IF NOT EXISTS`, `CREATE OR REPLACE`), like `prepare` itself.
pub(super) const SCHEMA_VERSION: &str = "1";

/// Key of the schema version in `spot_meta`.
pub(super) const META_SCHEMA: &str = "feed_schema";
/// Key of the stream generation in `spot_meta`.
pub(super) const META_GENERATION: &str = "feed_generation";
/// Key of the retention setting (seconds) in `spot_meta`.
pub(super) const META_RETENTION: &str = "feed_retention_secs";

/// The seven days of [`Capabilities::feed_retention`](spot_storage_contract::Capabilities).
pub(super) const DEFAULT_RETENTION_SECS: u64 = 7 * 24 * 60 * 60;

/// The tables, functions and guards of the outbox and the change feed.
///
/// - `spot_outbox` is insert-only: business code cannot update or delete a row. Pruning sets the
///   transaction-local `spot.feed_maintenance` flag.
/// - `spot_change` has one row per document per transaction, kept net by `spot_capture_change`.
/// - `*_pub` are the published positions `(generation, seq)`. A row is pending until it appears
///   there. `spot_feed_state` is the publisher's own bookkeeping; only the publisher touches it.
/// - Capture takes no shared counter or lock: the transaction id is a UUID kept in a
///   transaction-local setting, and rows are ordered by their capture time at publication.
pub(super) const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS spot_outbox (
    id uuid PRIMARY KEY,
    tx uuid NOT NULL,
    ordinal integer NOT NULL,
    topic text NOT NULL,
    subject_entity text NOT NULL,
    subject_id uuid NOT NULL,
    subject_version bigint,
    schema_version integer NOT NULL,
    payload text NOT NULL,
    captured_at timestamptz NOT NULL,
    xid xid8 NOT NULL
);
CREATE UNIQUE INDEX IF NOT EXISTS spot_outbox_tx_ordinal ON spot_outbox (tx, ordinal);
CREATE INDEX IF NOT EXISTS spot_outbox_xid ON spot_outbox (xid);

CREATE TABLE IF NOT EXISTS spot_outbox_pub (
    generation bigint NOT NULL,
    seq bigint NOT NULL,
    event_id uuid NOT NULL,
    published_at timestamptz NOT NULL,
    PRIMARY KEY (generation, seq)
);
CREATE UNIQUE INDEX IF NOT EXISTS spot_outbox_pub_event ON spot_outbox_pub (generation, event_id);
CREATE INDEX IF NOT EXISTS spot_outbox_pub_event_only ON spot_outbox_pub (event_id);

CREATE TABLE IF NOT EXISTS spot_change (
    tx uuid NOT NULL,
    entity text NOT NULL,
    doc uuid NOT NULL,
    op text NOT NULL CHECK (op IN ('I', 'U', 'D')),
    version bigint NOT NULL,
    captured_at timestamptz NOT NULL,
    xid xid8 NOT NULL,
    PRIMARY KEY (tx, entity, doc)
);
CREATE INDEX IF NOT EXISTS spot_change_xid ON spot_change (xid);

CREATE TABLE IF NOT EXISTS spot_change_pub (
    generation bigint NOT NULL,
    seq bigint NOT NULL,
    tx uuid NOT NULL,
    entity text NOT NULL,
    doc uuid NOT NULL,
    published_at timestamptz NOT NULL,
    PRIMARY KEY (generation, seq)
);
CREATE UNIQUE INDEX IF NOT EXISTS spot_change_pub_key ON spot_change_pub (generation, tx, entity, doc);
CREATE INDEX IF NOT EXISTS spot_change_pub_doc ON spot_change_pub (tx, entity, doc);

CREATE TABLE IF NOT EXISTS spot_feed_state (
    log text PRIMARY KEY,
    horizon xid8 NOT NULL,
    last_seq bigint NOT NULL,
    floor_seq bigint NOT NULL
);

CREATE OR REPLACE FUNCTION spot_feed_tx() RETURNS uuid LANGUAGE plpgsql AS $f$
DECLARE
    cur text := current_setting('spot.tx', true);
BEGIN
    IF cur IS NULL OR cur = '' THEN
        cur := uuidv7()::text;
        PERFORM set_config('spot.tx', cur, true);
    END IF;
    RETURN cur::uuid;
END
$f$;

CREATE OR REPLACE FUNCTION spot_capture_change() RETURNS trigger LANGUAGE plpgsql AS $f$
DECLARE
    t uuid := spot_feed_tx();
    prior text;
BEGIN
    IF TG_OP = 'DELETE' THEN
        SELECT op INTO prior FROM spot_change WHERE tx = t AND entity = TG_TABLE_NAME AND doc = OLD.id;
        IF prior = 'I' THEN
            DELETE FROM spot_change WHERE tx = t AND entity = TG_TABLE_NAME AND doc = OLD.id;
        ELSE
            INSERT INTO spot_change (tx, entity, doc, op, version, captured_at, xid)
            VALUES (t, TG_TABLE_NAME, OLD.id, 'D', OLD.version, clock_timestamp(), pg_current_xact_id())
            ON CONFLICT (tx, entity, doc) DO UPDATE
                SET op = 'D', version = EXCLUDED.version, captured_at = EXCLUDED.captured_at;
        END IF;
        RETURN OLD;
    ELSIF TG_OP = 'INSERT' THEN
        INSERT INTO spot_change (tx, entity, doc, op, version, captured_at, xid)
        VALUES (t, TG_TABLE_NAME, NEW.id, 'I', NEW.version, clock_timestamp(), pg_current_xact_id())
        ON CONFLICT (tx, entity, doc) DO UPDATE
            SET op = 'U', version = EXCLUDED.version, captured_at = EXCLUDED.captured_at;
        RETURN NEW;
    ELSE
        INSERT INTO spot_change (tx, entity, doc, op, version, captured_at, xid)
        VALUES (t, TG_TABLE_NAME, NEW.id, 'U', NEW.version, clock_timestamp(), pg_current_xact_id())
        ON CONFLICT (tx, entity, doc) DO UPDATE
            SET version = EXCLUDED.version, captured_at = EXCLUDED.captured_at;
        RETURN NEW;
    END IF;
END
$f$;

CREATE OR REPLACE FUNCTION spot_outbox_guard() RETURNS trigger LANGUAGE plpgsql AS $f$
BEGIN
    IF current_setting('spot.feed_maintenance', true) = 'on' THEN
        IF TG_OP = 'UPDATE' THEN
            RETURN NEW;
        ELSIF TG_OP = 'DELETE' THEN
            RETURN OLD;
        END IF;
        RETURN NULL;
    END IF;
    RAISE EXCEPTION 'spot_outbox is insert-only' USING ERRCODE = '55000';
END
$f$;

CREATE OR REPLACE TRIGGER spot_outbox_rows BEFORE UPDATE OR DELETE ON spot_outbox
    FOR EACH ROW EXECUTE FUNCTION spot_outbox_guard();
CREATE OR REPLACE TRIGGER spot_outbox_truncate BEFORE TRUNCATE ON spot_outbox
    FOR EACH STATEMENT EXECUTE FUNCTION spot_outbox_guard();
"#;

/// The state rows of both logs, created with the schema.
pub(super) const INITIAL_STATE: &str = "INSERT INTO spot_feed_state (log, horizon, last_seq, floor_seq) \
     VALUES ('outbox', '0', 0, 0), ('changes', '0', 0, 0) ON CONFLICT (log) DO NOTHING";

/// `CREATE TRIGGER` for the document table `table` (already quoted).
pub(super) fn capture_trigger(quoted_table: &str) -> String {
    format!(
        "CREATE TRIGGER spot_capture AFTER INSERT OR UPDATE OR DELETE ON {quoted_table} \
         FOR EACH ROW EXECUTE FUNCTION spot_capture_change()"
    )
}

/// The tables whose `spot_capture` trigger exists in the current schema.
pub(super) const TRIGGERED_TABLES: &str = "SELECT c.relname::text FROM pg_trigger t \
     JOIN pg_class c ON c.oid = t.tgrelid \
     WHERE t.tgname = 'spot_capture' AND NOT t.tgisinternal \
       AND c.relnamespace = (SELECT oid FROM pg_namespace WHERE nspname = current_schema())";

/// Inserts one outbox event with the next ordinal of its transaction. `$1` id, `$2` topic,
/// `$3`/`$4` subject entity and id, `$5` subject version, `$6` schema version, `$7` payload.
/// Zero rows affected means the event id exists.
pub(super) const ENQUEUE: &str = "INSERT INTO spot_outbox \
     (id, tx, ordinal, topic, subject_entity, subject_id, subject_version, schema_version, payload, \
      captured_at, xid) \
     SELECT $1, t.tx, \
            (SELECT coalesce(max(o.ordinal) + 1, 0) FROM spot_outbox o WHERE o.tx = t.tx), \
            $2, $3, $4, $5, $6, $7, clock_timestamp(), pg_current_xact_id() \
     FROM (SELECT spot_feed_tx() AS tx) t \
     ON CONFLICT (id) DO NOTHING";

/// Publishes the pending committed outbox entries (see `publish.rs` for why this is correct).
///
/// A pending entry is a visible `spot_outbox` row that has no position yet. Only rows at or above
/// the publisher's `horizon` can be pending: the horizon is the oldest still-running transaction
/// id seen by the previous publication, and everything below it had finished and was published
/// by then. Publication order is capture time, then transaction id, then ordinal.
pub(super) const PUBLISH_OUTBOX: &str = "\
WITH g AS (SELECT value::bigint AS generation FROM spot_meta WHERE key = 'feed_generation'), \
st AS (SELECT horizon, last_seq FROM spot_feed_state WHERE log = 'outbox'), \
snap AS (SELECT pg_snapshot_xmin(pg_current_snapshot()) AS xmin), \
pending AS ( \
    SELECT o.id, row_number() OVER (ORDER BY o.captured_at, o.xid, o.ordinal) AS n \
    FROM spot_outbox o, st, g \
    WHERE o.xid >= st.horizon \
      AND NOT EXISTS (SELECT 1 FROM spot_outbox_pub p \
                      WHERE p.generation = g.generation AND p.event_id = o.id)), \
ins AS ( \
    INSERT INTO spot_outbox_pub (generation, seq, event_id, published_at) \
    SELECT g.generation, st.last_seq + pending.n, pending.id, clock_timestamp() \
    FROM pending, g, st RETURNING seq) \
UPDATE spot_feed_state s \
SET horizon = greatest(s.horizon, (SELECT xmin FROM snap)), \
    last_seq = s.last_seq + (SELECT count(*) FROM ins) \
WHERE s.log = 'outbox'";

/// As [`PUBLISH_OUTBOX`], for the change feed; an entry is keyed by `(tx, entity, doc)`.
pub(super) const PUBLISH_CHANGES: &str = "\
WITH g AS (SELECT value::bigint AS generation FROM spot_meta WHERE key = 'feed_generation'), \
st AS (SELECT horizon, last_seq FROM spot_feed_state WHERE log = 'changes'), \
snap AS (SELECT pg_snapshot_xmin(pg_current_snapshot()) AS xmin), \
pending AS ( \
    SELECT c.tx, c.entity, c.doc, \
           row_number() OVER (ORDER BY c.captured_at, c.xid, c.entity, c.doc) AS n \
    FROM spot_change c, st, g \
    WHERE c.xid >= st.horizon \
      AND NOT EXISTS (SELECT 1 FROM spot_change_pub p \
                      WHERE p.generation = g.generation AND p.tx = c.tx \
                        AND p.entity = c.entity AND p.doc = c.doc)), \
ins AS ( \
    INSERT INTO spot_change_pub (generation, seq, tx, entity, doc, published_at) \
    SELECT g.generation, st.last_seq + pending.n, pending.tx, pending.entity, pending.doc, \
           clock_timestamp() \
    FROM pending, g, st RETURNING seq) \
UPDATE spot_feed_state s \
SET horizon = greatest(s.horizon, (SELECT xmin FROM snap)), \
    last_seq = s.last_seq + (SELECT count(*) FROM ins) \
WHERE s.log = 'changes'";

/// The generation and the bounds of one log: `$1` is `'outbox'` or `'changes'`.
pub(super) const STATE: &str = "SELECT (SELECT value::bigint FROM spot_meta WHERE key = 'feed_generation'), \
     last_seq, floor_seq FROM spot_feed_state WHERE log = $1";

/// Published outbox entries after `$2` in generation `$1`, at most `$3`.
pub(super) const READ_OUTBOX: &str = "\
SELECT p.seq, o.id, o.tx, o.ordinal, o.topic, o.subject_entity, o.subject_id, o.subject_version, \
       o.schema_version, o.payload \
FROM spot_outbox_pub p JOIN spot_outbox o ON o.id = p.event_id \
WHERE p.generation = $1 AND p.seq > $2 ORDER BY p.seq LIMIT $3";

/// Published change entries after `$2` in generation `$1`, at most `$3`.
pub(super) const READ_CHANGES: &str = "\
SELECT p.seq, c.tx, c.entity, c.doc, c.op, c.version \
FROM spot_change_pub p JOIN spot_change c ON c.tx = p.tx AND c.entity = p.entity AND c.doc = p.doc \
WHERE p.generation = $1 AND p.seq > $2 ORDER BY p.seq LIMIT $3";

/// Removes outbox entries published more than `$1` seconds ago (never the newest; a null `$1`
/// removes none) and every entry of an older generation, and raises the floor. Returns the number
/// of events removed.
pub(super) const PRUNE_OUTBOX: &str = "\
WITH g AS (SELECT value::bigint AS generation FROM spot_meta WHERE key = 'feed_generation'), \
gone AS ( \
    DELETE FROM spot_outbox_pub p USING g \
    WHERE p.generation <> g.generation \
       OR (p.published_at < clock_timestamp() - make_interval(secs => $1::double precision) \
           AND p.seq < (SELECT last_seq FROM spot_feed_state WHERE log = 'outbox')) \
    RETURNING p.generation, p.seq, p.event_id), \
del AS (DELETE FROM spot_outbox o USING gone WHERE o.id = gone.event_id RETURNING 1), \
fl AS ( \
    UPDATE spot_feed_state s \
    SET floor_seq = greatest(s.floor_seq, \
        coalesce((SELECT max(gone.seq) FROM gone, g WHERE gone.generation = g.generation), 0)) \
    WHERE s.log = 'outbox' RETURNING 1) \
SELECT (SELECT count(*) FROM del)";

/// As [`PRUNE_OUTBOX`], for the change feed.
pub(super) const PRUNE_CHANGES: &str = "\
WITH g AS (SELECT value::bigint AS generation FROM spot_meta WHERE key = 'feed_generation'), \
gone AS ( \
    DELETE FROM spot_change_pub p USING g \
    WHERE p.generation <> g.generation \
       OR (p.published_at < clock_timestamp() - make_interval(secs => $1::double precision) \
           AND p.seq < (SELECT last_seq FROM spot_feed_state WHERE log = 'changes')) \
    RETURNING p.generation, p.seq, p.tx, p.entity, p.doc), \
del AS ( \
    DELETE FROM spot_change c USING gone \
    WHERE c.tx = gone.tx AND c.entity = gone.entity AND c.doc = gone.doc RETURNING 1), \
fl AS ( \
    UPDATE spot_feed_state s \
    SET floor_seq = greatest(s.floor_seq, \
        coalesce((SELECT max(gone.seq) FROM gone, g WHERE gone.generation = g.generation), 0)) \
    WHERE s.log = 'changes' RETURNING 1) \
SELECT (SELECT count(*) FROM del)";

/// Starts a new generation: both logs begin again at sequence 0. Entries already published (in
/// earlier generations) are removed by the prune statements run right after, so what remains is
/// exactly the entries still pending, which the next publication numbers from 1; hence the
/// horizon goes back to the beginning.
pub(super) const RESET_STATE: &str =
    "UPDATE spot_feed_state SET horizon = '0', last_seq = 0, floor_seq = 0";
