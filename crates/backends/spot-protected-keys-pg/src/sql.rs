//! Every statement the store runs: fixed text with bound parameters. No identifier or fragment
//! is ever built from input (ADR-45; this crate is a storage crate and the only place SQL lives).
//!
//! Parameter `$1` is always the store's tenant, taken from its constructor, never from a method
//! argument.

/// Inserts a group's head at epoch 1 unless the group exists.
pub const INSERT_GROUP: &str = "\
INSERT INTO spot_key_group (tenant, group_name, current_epoch) VALUES ($1, $2, 1) \
ON CONFLICT DO NOTHING RETURNING current_epoch";

/// Inserts one epoch's wrapped key.
pub const INSERT_EPOCH: &str = "\
INSERT INTO spot_key_group_epoch \
    (tenant, group_name, epoch, supersedes, root_id, root_version, wrapped) \
VALUES ($1, $2, $3, $4, $5, $6, $7)";

/// The compare-and-set of the epoch head: moves `$3` to `$4` only if the head is `$3`. The
/// update takes the row lock, so two racing rotations have exactly one winner.
pub const BUMP_HEAD: &str = "\
UPDATE spot_key_group SET current_epoch = $4 \
WHERE tenant = $1 AND group_name = $2 AND current_epoch = $3 RETURNING current_epoch";

/// A group's head.
pub const HEAD: &str = "\
SELECT current_epoch FROM spot_key_group WHERE tenant = $1 AND group_name = $2";

/// Every group.
pub const GROUPS: &str = "\
SELECT group_name FROM spot_key_group WHERE tenant = $1 ORDER BY group_name COLLATE \"C\"";

/// One epoch's row.
pub const GROUP_KEY: &str = "\
SELECT group_name, epoch, root_id, root_version, wrapped, retired_at_ms \
FROM spot_key_group_epoch WHERE tenant = $1 AND group_name = $2 AND epoch = $3";

/// Erases an old epoch's wrapped key; never the head.
pub const RETIRE: &str = "\
UPDATE spot_key_group_epoch e SET wrapped = NULL, retired_at_ms = $4 \
WHERE e.tenant = $1 AND e.group_name = $2 AND e.epoch = $3 AND e.wrapped IS NOT NULL \
  AND e.epoch < (SELECT g.current_epoch FROM spot_key_group g \
                 WHERE g.tenant = $1 AND g.group_name = $2) \
RETURNING e.epoch";

/// Every live wrapped key.
pub const BACKUP: &str = "\
SELECT group_name, epoch, root_id, root_version, wrapped, retired_at_ms \
FROM spot_key_group_epoch WHERE tenant = $1 AND wrapped IS NOT NULL \
ORDER BY group_name COLLATE \"C\", epoch";

/// Registers a node, or replaces the key of one that is not revoked.
pub const UPSERT_NODE: &str = "\
INSERT INTO spot_key_node (tenant, node, status, root_id, root_version, wrapped, registered_at_ms) \
VALUES ($1, $2, 'active', $3, $4, $5, $6) \
ON CONFLICT (tenant, node) DO UPDATE SET status = 'active', \
    root_id = EXCLUDED.root_id, root_version = EXCLUDED.root_version, wrapped = EXCLUDED.wrapped \
WHERE spot_key_node.status <> 'revoked' \
RETURNING node";

/// One node.
pub const NODE: &str = "\
SELECT node, status, root_id, root_version, wrapped, registered_at_ms \
FROM spot_key_node WHERE tenant = $1 AND node = $2";

/// Suspends or reactivates a node that is not revoked.
pub const SET_STATUS: &str = "\
UPDATE spot_key_node SET status = $3 \
WHERE tenant = $1 AND node = $2 AND status <> 'revoked' RETURNING node";

/// Records a release in ONE statement: the node must be active (its row is share-locked, so a
/// concurrent revocation waits for this release or sees it), the epoch must be live, and the
/// node must have no revocation epoch at or before it. Nothing is inserted otherwise.
pub const RECORD_RELEASE: &str = "\
INSERT INTO spot_key_hold (tenant, node, group_name, epoch, lease_until_ms) \
SELECT $1::text, $2::text, $3::text, $4::bigint, $5::bigint FROM spot_key_node n \
WHERE n.tenant = $1 AND n.node = $2 AND n.status = 'active' \
  AND EXISTS (SELECT 1 FROM spot_key_group_epoch e \
              WHERE e.tenant = $1 AND e.group_name = $3 AND e.epoch = $4 AND e.wrapped IS NOT NULL) \
  AND NOT EXISTS (SELECT 1 FROM spot_key_revocation r \
                  WHERE r.tenant = $1 AND r.node = $2 AND r.group_name = $3 AND r.from_epoch <= $4) \
FOR SHARE OF n \
ON CONFLICT (tenant, node, group_name) DO UPDATE \
    SET epoch = EXCLUDED.epoch, lease_until_ms = EXCLUDED.lease_until_ms \
RETURNING epoch";

/// The keys released to a node.
pub const HOLDS: &str = "\
SELECT node, group_name, epoch, lease_until_ms FROM spot_key_hold \
WHERE tenant = $1 AND node = $2 ORDER BY group_name COLLATE \"C\"";

/// Marks a node revoked and erases its wrapped key. The row lock this takes orders it against
/// every release to the node.
pub const REVOKE_NODE: &str = "\
UPDATE spot_key_node SET status = 'revoked', wrapped = NULL, root_id = NULL, \
    root_version = NULL, revoked_at_ms = $3 \
WHERE tenant = $1 AND node = $2 AND status <> 'revoked' RETURNING node";

/// The groups a node holds.
pub const HOLD_GROUPS: &str = "\
SELECT group_name FROM spot_key_hold WHERE tenant = $1 AND node = $2";

/// Forgets a revoked node's holds.
pub const DELETE_HOLDS: &str = "\
DELETE FROM spot_key_hold WHERE tenant = $1 AND node = $2";

/// Records one group's step in a revocation.
pub const INSERT_REVOCATION: &str = "\
INSERT INTO spot_key_revocation (tenant, node, group_name, from_epoch, to_epoch, revoked_at_ms) \
VALUES ($1, $2, $3, $4, $5, $6)";

/// The epoch in force when a node was revoked, for one group.
pub const REVOCATION_EPOCH: &str = "\
SELECT from_epoch FROM spot_key_revocation \
WHERE tenant = $1 AND node = $2 AND group_name = $3";

/// Appends one history event.
pub const INSERT_EVENT: &str = "\
INSERT INTO spot_key_event (tenant, kind, group_name, node, epoch, to_epoch, detail, actor, at_ms) \
VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)";

/// A page of history.
pub const EVENTS: &str = "\
SELECT seq, kind, group_name, node, epoch, to_epoch, detail, actor, at_ms \
FROM spot_key_event WHERE tenant = $1 AND seq > $2 ORDER BY seq LIMIT $3";

/// Gives the application role its least-privilege grants (the role name is bound data).
pub const RESTRICT_ROLE: &str = "SELECT spot_key_restrict_role($1)";
