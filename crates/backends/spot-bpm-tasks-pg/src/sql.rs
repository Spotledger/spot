//! Every statement the store runs: fixed text with bound parameters. No identifier or fragment
//! is ever built from input (ADR-45; this crate is a storage crate and the only place SQL lives).
//!
//! Parameter `$1` is always the store's tenant, taken from its constructor, never from a method
//! argument.

/// The columns every task read selects, in the order `codec::task_from_row` reads them by name.
macro_rules! task_columns {
    () => {
        "task_id, case_id, node, status, version, body"
    };
}

/// Inserts a fresh task unless the `(case, node)` pair (or the task id) is already taken.
pub const INSERT_TASK: &str = "\
INSERT INTO spot_bpm_task \
    (tenant, task_id, case_id, node, process, status, version, event_seq, \
     assignees, roles, delegation, due, sla, created_at, body) \
VALUES ($1, $2, $3, $4, $5, $6, 1, 1, $7, $8, $9, $10, $11, $12, $13) \
ON CONFLICT DO NOTHING \
RETURNING task_id";

/// Inserts the `created` event of a fresh task (sequence 1).
pub const INSERT_FIRST_EVENT: &str = "\
INSERT INTO spot_bpm_task_event (tenant, task_id, seq, action, actor, at, body) \
VALUES ($1, $2, 1, $3, $4, $5, $6)";

/// One task by id.
pub const LOAD: &str = concat!(
    "SELECT ",
    task_columns!(),
    " FROM spot_bpm_task WHERE tenant = $1 AND task_id = $2"
);

/// One task by `(case, node)`.
pub const FIND_BY_NODE: &str = concat!(
    "SELECT ",
    task_columns!(),
    " FROM spot_bpm_task WHERE tenant = $1 AND case_id = $2 AND node = $3"
);

/// The version-checked update and the event insert, as ONE statement. The update takes the row
/// lock; a concurrent commit waits, re-checks the version and updates nothing, so the event
/// insert (which reads the update's row) writes nothing either. `event_seq` on the task row is
/// the counter that stamps the event, so two events can never share a sequence number.
pub const COMMIT: &str = "\
WITH upd AS ( \
    UPDATE spot_bpm_task SET \
        process = $6, status = $7, assignees = $8, roles = $9, delegation = $10, \
        due = $11, sla = $12, body = $13, \
        version = version + 1, \
        event_seq = event_seq + (CASE WHEN $14::boolean THEN 1 ELSE 0 END) \
    WHERE tenant = $1 AND task_id = $2 AND version = $3 AND case_id = $4 AND node = $5 \
    RETURNING event_seq \
), ev AS ( \
    INSERT INTO spot_bpm_task_event (tenant, task_id, seq, action, actor, at, body) \
    SELECT $1, $2, upd.event_seq, $15::text, $16::text, $17::timestamptz, \
           jsonb_set($18::jsonb, '{seq}', to_jsonb(upd.event_seq)) \
    FROM upd WHERE $14::boolean \
    RETURNING seq \
) \
SELECT count(*) AS updated FROM upd";

/// Why a commit updated nothing: the stored version and identity, when the task exists.
pub const DIAGNOSE: &str = "\
SELECT version, case_id, node FROM spot_bpm_task WHERE tenant = $1 AND task_id = $2";

/// The task's events in sequence order.
pub const EVENTS: &str = "\
SELECT task_id, seq, action, body FROM spot_bpm_task_event \
WHERE tenant = $1 AND task_id = $2 ORDER BY seq";

/// The tasks matching a `TaskFilter`; a `NULL` parameter means the condition is not set.
pub const LIST: &str = concat!(
    "SELECT ",
    task_columns!(),
    " FROM spot_bpm_task WHERE tenant = $1 \
      AND ($2::text[] IS NULL OR status = ANY($2::text[])) \
      AND ($3::text IS NULL OR case_id = $3::text) \
      AND ($4::text IS NULL OR process = $4::text) \
      AND ($5::text[] IS NULL OR assignees ?| $5::text[]) \
      AND ($6::timestamptz IS NULL OR (due IS NOT NULL AND due <= $6::timestamptz)) \
      ORDER BY task_id COLLATE \"C\""
);

/// The tasks holding any of the named roles, by id.
pub const LIST_BY_ROLE: &str = concat!(
    "SELECT ",
    task_columns!(),
    " FROM spot_bpm_task WHERE tenant = $1 AND roles ?| $2::text[] \
      ORDER BY task_id COLLATE \"C\""
);
