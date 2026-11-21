//! The statements of the store. Values reach SQL only as bound parameters (ADR-045); the only
//! text interpolated is none.

/// One run by id.
pub(crate) const SELECT_RUN: &str = "\
SELECT id, tenant, key, dag_key, dag_version, content_hash, status, started_at, ended_at, version, taken \
FROM spot_dag_run WHERE id = $1 AND tenant = $2";

/// One run by id, holding the row against a concurrent apply.
pub(crate) const SELECT_RUN_FOR_UPDATE: &str = "\
SELECT id, tenant, key, dag_key, dag_version, content_hash, status, started_at, ended_at, version, taken \
FROM spot_dag_run WHERE id = $1 AND tenant = $2 FOR UPDATE";

/// One run by business key.
pub(crate) const SELECT_RUN_BY_KEY: &str = "\
SELECT id, tenant, key, dag_key, dag_version, content_hash, status, started_at, ended_at, version, taken \
FROM spot_dag_run WHERE key = $1 AND tenant = $2";

/// Every node of one run.
pub(crate) const SELECT_NODES: &str = "\
SELECT node, tenant, status, attempts, output, failure, started_at, ended_at \
FROM spot_dag_node_state WHERE run_id = $1 AND tenant = $2 ORDER BY node COLLATE \"C\"";

/// A fresh run row.
pub(crate) const INSERT_RUN: &str = "\
INSERT INTO spot_dag_run \
    (id, tenant, key, dag_key, dag_version, content_hash, status, started_at, ended_at, version, taken) \
VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)";

/// A fresh node row.
pub(crate) const INSERT_NODE: &str = "\
INSERT INTO spot_dag_node_state \
    (run_id, tenant, node, status, attempts, output, failure, started_at, ended_at) \
VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)";

/// The run's status, end instant, version and taken edges after a transition.
pub(crate) const UPDATE_RUN: &str = "\
UPDATE spot_dag_run SET status = $2, ended_at = $3, version = $4, taken = $5 \
WHERE id = $1 AND tenant = $6";

/// One node's state after a transition.
pub(crate) const UPDATE_NODE: &str = "\
UPDATE spot_dag_node_state \
SET status = $3, attempts = $4, output = $5, failure = $6, started_at = $7, ended_at = $8 \
WHERE run_id = $1 AND node = $2 AND tenant = $9";

/// The runs with a ready node, locked so a second lister skips them.
pub(crate) const LIST_READY_RUNS: &str = "\
SELECT r.id FROM spot_dag_run r \
WHERE r.status = 'running' AND r.tenant = $1 \
  AND EXISTS (SELECT 1 FROM spot_dag_node_state n WHERE n.run_id = r.id AND n.tenant = r.tenant AND n.status = 'ready') \
ORDER BY r.id COLLATE \"C\" \
LIMIT $2 \
FOR UPDATE SKIP LOCKED";

/// The ready nodes of the listed runs, locked so a second lister skips them.
pub(crate) const LIST_READY_NODES: &str = "\
SELECT run_id, node FROM spot_dag_node_state \
WHERE run_id = ANY($1) AND tenant = $2 AND status = 'ready' \
ORDER BY run_id COLLATE \"C\", node COLLATE \"C\" \
FOR UPDATE SKIP LOCKED";
