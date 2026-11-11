//! The statements. Every one is a constant (or built once from constants); no value is ever
//! interpolated (ADR-45). Everything read is a published position: `spot_change_pub` for the
//! change feed and `spot_outbox_pub` for the journal's `doc_state` events.

/// Starts the one snapshot both streams are read in.
pub(crate) const BEGIN: &str = "BEGIN ISOLATION LEVEL REPEATABLE READ READ ONLY";
/// Ends it. Nothing was written, so ending is a rollback.
pub(crate) const END: &str = "ROLLBACK";
/// The transaction's own mode: `on` when the snapshot cannot write.
pub(crate) const READ_ONLY: &str = "SHOW transaction_read_only";

/// The stream generation (a fork or restore starts a new one).
pub(crate) const GENERATION: &str =
    "SELECT coalesce((SELECT value::bigint FROM spot_meta WHERE key = 'feed_generation'), 0)";

/// The newest published position of a stream in a generation: `$1` generation.
pub(crate) const FEED_HEAD: &str =
    "SELECT coalesce(max(seq), 0) FROM spot_change_pub WHERE generation = $1";
/// As [`FEED_HEAD`], for the events.
pub(crate) const EVENT_HEAD: &str =
    "SELECT coalesce(max(seq), 0) FROM spot_outbox_pub WHERE generation = $1";

/// The newest position published at least `$2` seconds before the snapshot's own instant: the
/// stable head, everything at or below which has had `$2` seconds to find its counterpart.
pub(crate) const FEED_STABLE: &str = "SELECT coalesce(max(seq), 0) FROM spot_change_pub \
     WHERE generation = $1 AND published_at <= now() - ($2::bigint * interval '1 second')";
/// As [`FEED_STABLE`], for the events.
pub(crate) const EVENT_STABLE: &str = "SELECT coalesce(max(seq), 0) FROM spot_outbox_pub \
     WHERE generation = $1 AND published_at <= now() - ($2::bigint * interval '1 second')";

/// Where a resumed scan starts reading, for the change feed: `$1` generation, `$2` the feed
/// watermark, `$3` the event watermark, `$4` the lag in seconds.
///
/// The scan starts `$4` seconds before the older of the two watermark entries' publication
/// instants, in both streams, so a document whose two sides straddle the watermark is compared
/// whole. It answers 0 (read from the beginning) when either watermark is 0 or no longer exists
/// (pruned): nothing is skipped silently.
pub(crate) const FEED_START: &str = "\
WITH anchor AS ( \
    SELECT CASE WHEN (SELECT count(*) FROM spot_change_pub WHERE generation = $1 AND seq = $2) = 1 \
                 AND (SELECT count(*) FROM spot_outbox_pub WHERE generation = $1 AND seq = $3) = 1 \
           THEN least( \
                (SELECT published_at FROM spot_change_pub WHERE generation = $1 AND seq = $2), \
                (SELECT published_at FROM spot_outbox_pub WHERE generation = $1 AND seq = $3)) \
           END AS at) \
SELECT coalesce((SELECT min(s.seq) - 1 FROM spot_change_pub s, anchor a \
    WHERE s.generation = $1 AND a.at IS NOT NULL \
      AND s.published_at >= a.at - ($4::bigint * interval '1 second')), 0)";
/// As [`FEED_START`], for the events.
pub(crate) const EVENT_START: &str = "\
WITH anchor AS ( \
    SELECT CASE WHEN (SELECT count(*) FROM spot_change_pub WHERE generation = $1 AND seq = $2) = 1 \
                 AND (SELECT count(*) FROM spot_outbox_pub WHERE generation = $1 AND seq = $3) = 1 \
           THEN least( \
                (SELECT published_at FROM spot_change_pub WHERE generation = $1 AND seq = $2), \
                (SELECT published_at FROM spot_outbox_pub WHERE generation = $1 AND seq = $3)) \
           END AS at) \
SELECT coalesce((SELECT min(s.seq) - 1 FROM spot_outbox_pub s, anchor a \
    WHERE s.generation = $1 AND a.at IS NOT NULL \
      AND s.published_at >= a.at - ($4::bigint * interval '1 second')), 0)";

/// Published change entries after `$2` in generation `$1`, at most `$3`: position, entity,
/// document id, committed version and the net operation.
pub(crate) const FEED_PAGE: &str = "\
SELECT p.seq, c.entity, c.doc::text, c.version, c.op \
FROM spot_change_pub p JOIN spot_change c ON c.tx = p.tx AND c.entity = p.entity AND c.doc = p.doc \
WHERE p.generation = $1 AND p.seq > $2 ORDER BY p.seq LIMIT $3";

/// Published `doc_state` events after `$2` in generation `$1`, at most `$3`: position, entity,
/// document id, version and the operation the event records. The payload is not selected: only
/// its operation is extracted, so a page never carries an image.
pub(crate) const EVENT_PAGE: &str = "\
SELECT p.seq, o.subject_entity, o.subject_id::text, o.subject_version, \
       (o.payload::jsonb) -> 'data' ->> 'op' \
FROM spot_outbox_pub p JOIN spot_outbox o ON o.id = p.event_id \
WHERE p.generation = $1 AND p.seq > $2 AND o.topic = 'doc_state' ORDER BY p.seq LIMIT $3";

/// The stored watermarks: source, generation, position.
pub(crate) const WATERMARKS: &str =
    "SELECT source, generation, seq FROM spot_capture_watermark ORDER BY source";

/// Stores one watermark: `$1` source, `$2` generation, `$3` position. The instant is the
/// database's own. Within one generation a watermark only moves forward, so two nodes of a hub
/// that both scan can never move each other's back.
pub(crate) const SAVE_WATERMARK: &str = "\
INSERT INTO spot_capture_watermark (source, generation, seq, updated_at) \
VALUES ($1, $2, $3, now()) \
ON CONFLICT (source) DO UPDATE \
SET seq = CASE WHEN spot_capture_watermark.generation = EXCLUDED.generation \
                THEN greatest(spot_capture_watermark.seq, EXCLUDED.seq) ELSE EXCLUDED.seq END, \
    generation = EXCLUDED.generation, \
    updated_at = EXCLUDED.updated_at";

/// Source name of the change feed's watermark.
pub(crate) const SOURCE_FEED: &str = "change_feed";
/// Source name of the journal's watermark.
pub(crate) const SOURCE_EVENTS: &str = "doc_state";
