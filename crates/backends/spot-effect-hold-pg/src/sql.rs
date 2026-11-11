//! The statements of the ledger. Values reach SQL only as bound parameters (ADR-045); no text is
//! interpolated into any statement here.

/// One effect's id, by its deduplication key.
pub(crate) const SELECT_EFFECT_ID: &str =
    "SELECT id FROM spot_effect_hold_effect WHERE site = $1 AND effect_key = $2";

/// The next sequence number for a site: `1` the first time, then one past the last allocated.
///
/// The conflicting update takes the counter row's lock, so appends to one site serialise here and
/// the numbers they get are the order the ledger lists them in. A transaction that rolls back
/// leaves a gap, never a reused number.
pub(crate) const NEXT_SEQUENCE: &str = "\
INSERT INTO spot_effect_hold_sequence (site, last_sequence) VALUES ($1, 1) \
ON CONFLICT (site) DO UPDATE \
SET last_sequence = spot_effect_hold_sequence.last_sequence + 1 \
RETURNING last_sequence";

/// Appends a held effect unless its `(site, key)` is already present.
///
/// The unique constraint is what deduplicates; `DO NOTHING` returns no row for a duplicate, and
/// the caller then reads the existing id. This keeps the original row's id, sequence and state.
pub(crate) const INSERT_EFFECT: &str = "\
INSERT INTO spot_effect_hold_effect \
    (site, seq, effect_key, effect_class, effect_class_key, kind, source, source_event, \
     created_at, payload_ref, migration_id, status, note) \
VALUES ($1, $2, $3, $4::text::jsonb, $5, $6, $7, $8, $9, $10::text::jsonb, $11, $12, $13) \
ON CONFLICT (site, effect_key) DO NOTHING \
RETURNING id";

/// A site's effects in the requested states, in the original (sequence) order.
pub(crate) const LIST_EFFECTS: &str = "\
SELECT id, seq, site, effect_key, effect_class::text, kind, source, source_event, \
       created_at, payload_ref::text, migration_id, status, note, attempts \
FROM spot_effect_hold_effect \
WHERE site = $1 AND status = ANY($2) \
ORDER BY seq";

/// One effect's compare-and-set: `expected -> next`, or nothing.
///
/// The `WHERE status = $2` is the whole guarantee: PostgreSQL re-checks it against the committed
/// row after waiting for a competing update, so of any number of nodes that read the same state
/// exactly one updates a row. `$5` is `1` for a transition into `releasing` (it counts an attempt)
/// and `0` otherwise.
pub(crate) const TRANSITION_EFFECT: &str = "\
UPDATE spot_effect_hold_effect \
SET status = $3, note = $4, attempts = attempts + $5 \
WHERE id = $1 AND status = $2";

/// One effect's current status, to report what a stale transition found.
pub(crate) const SELECT_EFFECT_STATUS: &str =
    "SELECT status FROM spot_effect_hold_effect WHERE id = $1";

/// A site's stored hold state.
pub(crate) const SELECT_HOLD: &str =
    "SELECT state::text FROM spot_effect_hold_state WHERE site = $1";

/// The hold state of a site that has never had a row: `Open -> new`.
///
/// The insert creates the row; the conflicting update applies only when the stored state is
/// exactly `expected`, so a compare-and-set against `Open` cannot overwrite a live hold.
pub(crate) const CAS_INSERT_HOLD: &str = "\
INSERT INTO spot_effect_hold_state AS hold (site, state) VALUES ($1, $2::text::jsonb) \
ON CONFLICT (site) DO UPDATE SET state = EXCLUDED.state \
WHERE hold.state = $3::text::jsonb";

/// The hold state of a site that already has a row: `expected -> new`, or nothing.
///
/// A site with no row has never been held, so it is `Open` and only [`CAS_INSERT_HOLD`] applies;
/// this statement therefore never creates a row. That is what makes a compare-and-set from
/// `Held` on an unknown site fail instead of succeeding.
pub(crate) const CAS_UPDATE_HOLD: &str = "\
UPDATE spot_effect_hold_state SET state = $2::text::jsonb \
WHERE site = $1 AND state = $3::text::jsonb";

/// Records a release's progress, replacing the previous checkpoint for the migration.
pub(crate) const UPSERT_CHECKPOINT: &str = "\
INSERT INTO spot_effect_hold_checkpoint \
    (site, migration_id, dispatched, last_sequence, needs_confirmation) \
VALUES ($1, $2, $3::text::numeric, $4::text::numeric, $5::text::numeric) \
ON CONFLICT (site, migration_id) DO UPDATE \
SET dispatched = EXCLUDED.dispatched, \
    last_sequence = EXCLUDED.last_sequence, \
    needs_confirmation = EXCLUDED.needs_confirmation";

/// A release's last checkpoint.
pub(crate) const SELECT_CHECKPOINT: &str = "\
SELECT dispatched::text, last_sequence::text, needs_confirmation::text \
FROM spot_effect_hold_checkpoint WHERE site = $1 AND migration_id = $2";
