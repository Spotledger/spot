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
