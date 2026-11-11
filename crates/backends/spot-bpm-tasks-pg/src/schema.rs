//! The DDL of the task inbox tables, and its indexes.
//!
//! | Table | Holds |
//! | --- | --- |
//! | `spot_bpm_task` | one row per task: identity, status, version, the resolved assignees, the role names and the delegation as `jsonb`, the due instant and the SLA policy, and the whole task as `body` |
//! | `spot_bpm_task_event` | the append-only audit trail, one row per action; a trigger refuses `UPDATE`, `DELETE` and `TRUNCATE` |
//!
//! `body` is the source of truth a read decodes; the other columns are the same facts spelled
//! for the indexes, written from the same value in the same statement. A read checks that they
//! agree and refuses a row where they do not (a corrupt row is an error, never skipped).
//!
//! # Tenancy
//!
//! Every table has `tenant` as the first column of its primary key (and of the `(case, node)`
//! unique key), so two tenants can hold the same task id and the same `(case, node)` pair.
//!
//! # Indexes
//!
//! * primary key `(tenant, task_id)`: `load`, `commit`, and the event foreign key.
//! * unique `(tenant, case_id, node)`: `find_by_node`, and the idempotent `create`.
//! * `spot_bpm_task_assignees_idx` GIN on `assignees`: the by-assignee inbox view.
//! * `spot_bpm_task_roles_idx` GIN on `roles`: the by-role inbox view.
//! * `spot_bpm_task_status_idx (tenant, status, task_id)`: the supervisor view (every task of a
//!   status) and the default sort.
//! * `spot_bpm_task_due_idx (tenant, due)` partial on tasks with a due instant: overdue.
//! * `spot_bpm_task_event` primary key `(tenant, task_id, seq)`: `events`, in order.

use tokio_postgres::Client;

/// The lock key that serialises concurrent `prepare` calls (transaction scoped).
const PREPARE_LOCK: i64 = 0x6270_6d74_6b01;

/// The idempotent DDL. Every statement is `IF NOT EXISTS`, `OR REPLACE` or guarded.
pub const DDL: &str = r"
CREATE TABLE IF NOT EXISTS spot_bpm_task (
    tenant     text NOT NULL CHECK (tenant <> ''),
    task_id    text NOT NULL,
    case_id    text NOT NULL,
    node       text NOT NULL,
    process    text NOT NULL,
    status     text NOT NULL
        CHECK (status IN ('open', 'claimed', 'completed', 'cancelled', 'expired')),
    version    bigint NOT NULL CHECK (version >= 1),
    event_seq  bigint NOT NULL CHECK (event_seq >= 0),
    assignees  jsonb NOT NULL CHECK (jsonb_typeof(assignees) = 'array'),
    roles      jsonb NOT NULL CHECK (jsonb_typeof(roles) = 'array'),
    delegation jsonb CHECK (delegation IS NULL OR jsonb_typeof(delegation) = 'object'),
    due        timestamptz,
    sla        jsonb CHECK (sla IS NULL OR jsonb_typeof(sla) = 'object'),
    created_at timestamptz NOT NULL,
    body       jsonb NOT NULL CHECK (jsonb_typeof(body) = 'object'),
    PRIMARY KEY (tenant, task_id)
);

CREATE UNIQUE INDEX IF NOT EXISTS spot_bpm_task_node_idx
    ON spot_bpm_task (tenant, case_id, node);

CREATE INDEX IF NOT EXISTS spot_bpm_task_assignees_idx
    ON spot_bpm_task USING gin (assignees);

CREATE INDEX IF NOT EXISTS spot_bpm_task_roles_idx
    ON spot_bpm_task USING gin (roles);

CREATE INDEX IF NOT EXISTS spot_bpm_task_status_idx
    ON spot_bpm_task (tenant, status, task_id);

CREATE INDEX IF NOT EXISTS spot_bpm_task_due_idx
    ON spot_bpm_task (tenant, due) WHERE due IS NOT NULL;

CREATE TABLE IF NOT EXISTS spot_bpm_task_event (
    tenant  text NOT NULL,
    task_id text NOT NULL,
    seq     bigint NOT NULL CHECK (seq >= 1),
    action  text NOT NULL,
    actor   text NOT NULL,
    at      timestamptz NOT NULL,
    body    jsonb NOT NULL CHECK (jsonb_typeof(body) = 'object'),
    PRIMARY KEY (tenant, task_id, seq),
    FOREIGN KEY (tenant, task_id) REFERENCES spot_bpm_task (tenant, task_id)
);

CREATE OR REPLACE FUNCTION spot_bpm_task_event_immutable()
RETURNS trigger LANGUAGE plpgsql AS $fn$
BEGIN
    RAISE EXCEPTION 'the task audit trail is append-only';
END
$fn$;

DROP TRIGGER IF EXISTS spot_bpm_task_event_append_only ON spot_bpm_task_event;
CREATE TRIGGER spot_bpm_task_event_append_only
    BEFORE UPDATE OR DELETE ON spot_bpm_task_event
    FOR EACH ROW EXECUTE FUNCTION spot_bpm_task_event_immutable();

DROP TRIGGER IF EXISTS spot_bpm_task_event_no_truncate ON spot_bpm_task_event;
CREATE TRIGGER spot_bpm_task_event_no_truncate
    BEFORE TRUNCATE ON spot_bpm_task_event
    FOR EACH STATEMENT EXECUTE FUNCTION spot_bpm_task_event_immutable();
";

/// Creates the tables in one transaction under a transaction-scoped advisory lock.
///
/// # Errors
/// The driver's error; the caller maps it to a fixed sentence.
pub(crate) async fn prepare(client: &mut Client) -> Result<(), tokio_postgres::Error> {
    let tx = client.transaction().await?;
    tx.execute("SELECT pg_advisory_xact_lock($1)", &[&PREPARE_LOCK])
        .await?;
    tx.batch_execute(DDL).await?;
    tx.commit().await
}
