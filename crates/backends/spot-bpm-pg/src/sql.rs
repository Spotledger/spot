//! The statements of the store. Values reach SQL only as bound parameters (ADR-045); the only
//! text interpolated is none.

/// One Case by id, scoped to its tenant.
pub const SELECT_CASE: &str = "\
SELECT case_id, tenant, business_key, dag_key, dag_version, content_hash, status, version, \
       started_at, ended_at, initiator, nodes, taken, children, has_ready, has_children \
FROM spot_bpm_case WHERE case_id = $1 AND tenant = $2";

/// One Case by id, holding the row against a concurrent commit.
pub const SELECT_CASE_FOR_UPDATE: &str = "\
SELECT case_id, tenant, business_key, dag_key, dag_version, content_hash, status, version, \
       started_at, ended_at, initiator, nodes, taken, children, has_ready, has_children \
FROM spot_bpm_case WHERE case_id = $1 AND tenant = $2 FOR UPDATE";

/// One Case by business key, scoped to its tenant.
pub const SELECT_CASE_BY_KEY: &str = "\
SELECT case_id, tenant, business_key, dag_key, dag_version, content_hash, status, version, \
       started_at, ended_at, initiator, nodes, taken, children, has_ready, has_children \
FROM spot_bpm_case WHERE tenant = $1 AND business_key = $2";

/// Every node row of one Case, ordered by node.
pub const SELECT_NODES: &str = "\
SELECT case_id, tenant, node, attempt, state, output, branches, failure, started_at, \
       not_before, is_retry, due, event, human \
FROM spot_bpm_node_outcome WHERE case_id = $1 ORDER BY node COLLATE \"C\"";

/// A Case's variables.
pub const SELECT_VAR: &str = "\
SELECT variables FROM spot_bpm_case_var WHERE case_id = $1";

/// A fresh Case row.
pub const INSERT_CASE: &str = "\
INSERT INTO spot_bpm_case \
    (case_id, tenant, business_key, dag_key, dag_version, content_hash, status, version, \
     started_at, ended_at, initiator, nodes, taken, children, has_ready, has_children) \
VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16)";

/// The Case row after a transition: status, end instant, version, node map, taken edges, child
/// links and the two runnable flags.
pub const UPDATE_CASE: &str = "\
UPDATE spot_bpm_case SET status = $3, ended_at = $4, version = $5, nodes = $6, taken = $7, \
       children = $8, has_ready = $9, has_children = $10 \
WHERE case_id = $1 AND tenant = $2";

/// The Case's variables, inserted or replaced.
pub const UPSERT_VAR: &str = "\
INSERT INTO spot_bpm_case_var (case_id, tenant, variables) VALUES ($1, $2, $3) \
ON CONFLICT (case_id) DO UPDATE SET variables = EXCLUDED.variables";

/// Every node row of one node, removed before the merged row is written.
pub const DELETE_NODE: &str = "\
DELETE FROM spot_bpm_node_outcome WHERE case_id = $1 AND node = $2";

/// One node row: the attempt, the wait and the tenant.
pub const INSERT_NODE: &str = "\
INSERT INTO spot_bpm_node_outcome \
    (case_id, tenant, node, attempt, state, output, branches, failure, started_at, not_before, \
     is_retry, due, event, human) \
VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14)";

/// The Cases that may have work: a ready node, a child link, or a timer due at `$2`.
pub const LIST_RUNNABLE: &str = "\
SELECT case_id FROM spot_bpm_case \
WHERE tenant = $1 AND status = 'running' \
  AND (has_ready OR has_children \
       OR EXISTS (SELECT 1 FROM spot_bpm_node_outcome o \
                  WHERE o.case_id = spot_bpm_case.case_id \
                    AND o.due IS NOT NULL AND o.due <= $2)) \
ORDER BY case_id COLLATE \"C\" \
LIMIT $3";

/// The nodes waiting on one event key: "Cases waiting on event X".
pub const WAITING_ON_EVENT: &str = "\
SELECT case_id, node FROM spot_bpm_node_outcome \
WHERE tenant = $1 AND event = $2 \
ORDER BY case_id COLLATE \"C\", node COLLATE \"C\"";

/// The nodes whose timer is due at or before `$1` in tenant `$2`: "timers due".
pub const DUE_TIMERS: &str = "\
SELECT case_id, node FROM spot_bpm_node_outcome \
WHERE due IS NOT NULL AND due <= $1 AND tenant = $2 \
ORDER BY due, case_id COLLATE \"C\", node COLLATE \"C\"";

/// The node-run log record of one `(case, node, attempt, state)` in a tenant, if already stored:
/// the replay check that runs before a sequence number is reserved, so a replay burns none.
pub const NODE_RUN_EXISTS: &str = "\
SELECT 1 FROM spot_bpm_node_run_event \
WHERE tenant = $1 AND case_id = $2 AND node = $3 AND attempt = $4 AND state = $5";

/// Reserves `$2` consecutive sequence numbers of tenant `$1` and returns the new last one. The
/// counter row is locked until the transaction ends, so this is the **last** statement of a
/// commit: sequence numbers then follow commit order.
pub const NODE_RUN_RESERVE: &str = "\
INSERT INTO spot_bpm_node_run_seq (tenant, last) VALUES ($1, $2) \
ON CONFLICT (tenant) DO UPDATE SET last = spot_bpm_node_run_seq.last + EXCLUDED.last \
RETURNING last";

/// One node-run record. A repeat of the replay key adds nothing; a clash of `(tenant, seq)` is an
/// error, never swallowed.
pub const INSERT_NODE_RUN: &str = "\
INSERT INTO spot_bpm_node_run_event \
    (tenant, seq, case_id, dag_key, dag_version, case_version, node, attempt, state, \"at\", \"wait\", \
     failure_code, is_retry, human) \
VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14) \
ON CONFLICT (tenant, case_id, node, attempt, state) DO NOTHING";

/// Up to `$3` records of tenant `$1` after position `$2`, in order: a range scan of the
/// `(tenant, seq)` primary key.
pub const LIST_NODE_RUNS: &str = "\
SELECT seq, case_id, dag_key, dag_version, case_version, node, attempt, state, \"at\", \"wait\", \
       failure_code, is_retry, human \
FROM spot_bpm_node_run_event \
WHERE tenant = $1 AND seq > $2 \
ORDER BY seq \
LIMIT $3";

/// A consumer's checkpoint.
pub const SELECT_NODE_RUN_CHECKPOINT: &str = "\
SELECT seq, state FROM spot_bpm_node_run_consumer WHERE tenant = $1 AND consumer = $2";

/// A consumer's first checkpoint (no row yet).
pub const INSERT_NODE_RUN_CHECKPOINT: &str = "\
INSERT INTO spot_bpm_node_run_consumer (tenant, consumer, seq, state, updated_at) \
VALUES ($1, $2, $3, $4, now()) \
ON CONFLICT (tenant, consumer) DO NOTHING";

/// The compare-and-set: moves the checkpoint only while its seq is still `$3`.
pub const UPDATE_NODE_RUN_CHECKPOINT: &str = "\
UPDATE spot_bpm_node_run_consumer SET seq = $4, state = $5, updated_at = now() \
WHERE tenant = $1 AND consumer = $2 AND seq = $3";

/// Deletes tenant `$1`'s records below `$2`, never above the slowest consumer's checkpoint: a
/// record a consumer has not folded yet (`seq > min checkpoint`) is kept.
pub const PRUNE_NODE_RUNS: &str = "\
DELETE FROM spot_bpm_node_run_event \
WHERE tenant = $1 AND seq < LEAST($2::bigint, \
    COALESCE((SELECT min(c.seq) + 1 FROM spot_bpm_node_run_consumer c WHERE c.tenant = $1), \
             $2::bigint))";

/// Every statement of the node-run log. Each is tenant-scoped: it filters on `tenant = $1` or, for
/// an insert, writes `tenant` from `$1`. `tests/node_run_log.rs` asserts it.
pub const NODE_RUN_STATEMENTS: [&str; 8] = [
    NODE_RUN_EXISTS,
    NODE_RUN_RESERVE,
    INSERT_NODE_RUN,
    LIST_NODE_RUNS,
    SELECT_NODE_RUN_CHECKPOINT,
    INSERT_NODE_RUN_CHECKPOINT,
    UPDATE_NODE_RUN_CHECKPOINT,
    PRUNE_NODE_RUNS,
];

/// The Case row a migration is about to rewrite, held against a concurrent commit: the pin, the
/// status, the revision the plan saw, the node map and whether sub-process children exist.
pub const SELECT_CASE_FOR_MIGRATION: &str = "\
SELECT dag_key, dag_version, status, version, nodes, has_children \
FROM spot_bpm_case WHERE case_id = $1 AND tenant = $2 FOR UPDATE";

/// The attempt state and whether a timer is set, for every node row of one Case.
pub const SELECT_ATTEMPTS_FOR_MIGRATION: &str = "\
SELECT state, (due IS NOT NULL) AS timed FROM spot_bpm_node_outcome \
WHERE case_id = $1 AND tenant = $2";

/// The Case row after a migration: the new pin, the migrated node map and taken edges, the next
/// revision and the runnable flag. The status, the children and the business key never change.
pub const UPDATE_CASE_MIGRATED: &str = "\
UPDATE spot_bpm_case SET dag_version = $3, content_hash = $4, nodes = $5, taken = $6, \
       version = $7, has_ready = $8 \
WHERE case_id = $1 AND tenant = $2";

/// The per-Run audit row of a migration (the table is created by the host, `spot-cli`).
pub const INSERT_MIGRATION_AUDIT: &str = "\
INSERT INTO spot_bpm_migration_audit (tenant, migration_id, case_id, plan_digest, payload, at) \
VALUES ($1, $2, $3, $4, $5, $6)";

/// One more Run moved by a migration (the table is created by the host, `spot-cli`).
pub const COUNT_MIGRATION_MOVED: &str = "\
UPDATE spot_bpm_migration SET moved = moved + 1 WHERE tenant = $1 AND id = $2";

/// How many running Cases of one process sit on each version.
pub const COUNT_RUNNING_BY_VERSION: &str = "\
SELECT dag_version, count(*) AS running FROM spot_bpm_case \
WHERE tenant = $1 AND dag_key = $2 AND status = 'running' GROUP BY dag_version";

/// One page of the running Cases of one process version, after a cursor, in id order.
pub const LIST_RUNNING_ON_VERSION: &str = "\
SELECT case_id FROM spot_bpm_case WHERE tenant = $1 AND dag_key = $2 AND dag_version = $3 \
AND status = 'running' AND case_id > $4 ORDER BY case_id LIMIT $5";

/// One page of the Cases a viewer may see, newest first, after the keyset `($8, $9)` when one is
/// given. Visibility is applied here, before the cut: everything for a supervisor, otherwise the
/// Cases the viewer started or the ones in `$4` (the Cases of the viewer's tasks). `$7` keeps
/// only the viewer's own.
pub const SELECT_VISIBLE_CASES: &str = "\
SELECT case_id, started_at FROM spot_bpm_case \
WHERE tenant = $1 \
  AND ($2::boolean OR initiator = $3::text OR case_id = ANY($4::text[])) \
  AND ($5::text IS NULL OR dag_key = $5::text) \
  AND ($6::text IS NULL OR status = $6::text) \
  AND (NOT $7::boolean OR initiator = $3::text) \
  AND ($8::timestamptz IS NULL OR (started_at, case_id) < ($8::timestamptz, $9::text)) \
ORDER BY started_at DESC, case_id DESC LIMIT $10::bigint";
